//! 通过真实 SessionManager reader 和 writer 验证私有快照隔离及一次性追加。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kerminal_application::AppResult;
use kerminal_application::sessions::logging::{
    OutputRecordResult, SessionLogState, SessionOutputRecorder,
};
use kerminal_application::sessions::{
    CreateSessionRequest, PlainTextSnapshotRequest, SessionService, SessionTransport,
    SessionTransportControl, SessionTransportFactory, SessionTransportOpenError,
    ShellEditingAvailability, ShellEditingSnapshot, ShellEditingUnavailableReason,
    TerminalCommandAvailability, TerminalCommandUnavailableReason, TerminalResizeCapability,
    UiInputProjection, UiSessionBinding, UiTerminalInputRequest,
};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::identity::{HostId, PaneId, SessionId, TabId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionOwner, SessionTarget, SshSessionTarget,
    TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};
use kerminal_terminal::shell_editing::{MAX_SHELL_EDITING_FRAME_BYTES, SHELL_EDITING_FRAME_PREFIX};
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf, ReadHalf, WriteHalf,
};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

const NONCE: &str = "0123456789abcdef0123456789abcdef";
const PROBE: &[u8] = b"\x1b[24;8~";
const HIDE_AND_QUERY: &[u8] = b"\x1b[?25l\x1b[6n";
const BASH_ERASE_AND_QUERY: &[u8] = b"\r\x1b[K\x1b[6n";

#[derive(Default)]
struct Recorder(StdMutex<Vec<u8>>);

#[async_trait]
impl SessionOutputRecorder for Recorder {
    /// 保留原始输出以断言过滤发生在日志边界之前，而不是只隐藏界面文本。
    ///
    /// @author kongweiguang
    fn try_record_output(&self, _: &SessionId, _: u64, bytes: &[u8]) -> OutputRecordResult {
        self.0
            .lock()
            .expect("recorder lock")
            .extend_from_slice(bytes);
        OutputRecordResult::Queued
    }

    /// 测试 recorder 无后台 worker，关闭时只返回未启用状态。
    ///
    /// @author kongweiguang
    async fn finish_session(&self, _: &SessionId, _: Instant) -> AppResult<SessionLogState> {
        Ok(SessionLogState::inactive())
    }

    /// 不创建后台资源，因此应用关闭无需附加等待。
    ///
    /// @author kongweiguang
    async fn shutdown_until(&self, _: Instant) -> AppResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Factory {
    peer: Mutex<Option<DuplexStream>>,
    suffix_gate: Option<Arc<SuffixGate>>,
    /// 启用 Windows ConPTY 能力，仅用于验证 reader 合批与私有帧过滤的真实接线。
    conpty: bool,
    ssh_powershell: bool,
    ssh_bash: bool,
    write_gate: Option<Arc<WriteGate>>,
}

/// 让 reader 在 transport 写上产生确定性背压，用来区分“批次已处理完”和“只处理了首段”。
#[derive(Default)]
struct WriteGate {
    blocked: AtomicBool,
    hide_read: Notify,
    waiting: Notify,
    waker: StdMutex<Option<Waker>>,
}

impl WriteGate {
    /// 解除背压并唤醒被挂起的 writer，避免测试结束时留下阻塞的 reader task。
    ///
    /// @author kongweiguang
    fn release(&self) {
        self.blocked.store(false, Ordering::Release);
        if let Some(waker) = self.waker.lock().expect("gate waker").take() {
            waker.wake();
        }
    }
}

/// 只阻塞被测后缀，探测帧仍走真实 writer，避免通过人为抢锁模拟不可能的时序。
#[derive(Default)]
struct SuffixGate {
    blocked: AtomicBool,
    write_prefix: AtomicBool,
    fail_flush: AtomicBool,
    suffix_written: AtomicBool,
    waiting: Notify,
}

struct ObservedReader {
    inner: ReadHalf<DuplexStream>,
    gate: Option<Arc<WriteGate>>,
}

impl AsyncRead for ObservedReader {
    /// 通知首段确实已读取，让测试在发送恢复段前建立底层分片边界。
    ///
    /// @author kongweiguang
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let offset = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Ok(())))
            && let Some(gate) = &self.gate
            && [HIDE_AND_QUERY, BASH_ERASE_AND_QUERY].contains(&&buffer.filled()[offset..])
        {
            gate.hide_read.notify_one();
        }
        result
    }
}

struct GatedWriter {
    inner: WriteHalf<DuplexStream>,
    suffix_gate: Option<Arc<SuffixGate>>,
    write_gate: Option<Arc<WriteGate>>,
}

impl AsyncWrite for GatedWriter {
    /// 根据测试模式在首字节前或首字节后制造背压，并保持探测帧的真实 FIFO 顺序。
    ///
    /// @author kongweiguang
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(gate) = &self.write_gate
            && gate.blocked.load(Ordering::Acquire)
        {
            // 先保存 waker 再通知测试，确保测试解除背压时一定能重新调度 writer。
            *gate.waker.lock().expect("gate waker") = Some(cx.waker().clone());
            gate.waiting.notify_one();
            return Poll::Pending;
        }
        if let Some(gate) = self.suffix_gate.clone() {
            if gate.blocked.load(Ordering::Acquire) && (bytes == b"tus" || bytes == b"us") {
                if bytes == b"tus" && gate.write_prefix.load(Ordering::Acquire) {
                    return Pin::new(&mut self.inner).poll_write(cx, &bytes[..1]);
                }
                gate.waiting.notify_one();
                return Poll::Pending;
            }
            let written = Pin::new(&mut self.inner).poll_write(cx, bytes);
            if bytes == b"tus" && matches!(&written, Poll::Ready(Ok(count)) if *count > 0) {
                gate.suffix_written.store(true, Ordering::Release);
            }
            return written;
        }
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    /// 仅可在测试指定的后缀之后注入 flush 故障，其他帧仍使用原双工流。
    ///
    /// @author kongweiguang
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(gate) = &self.suffix_gate
            && gate.fail_flush.load(Ordering::Acquire)
            && gate.suffix_written.swap(false, Ordering::AcqRel)
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected suffix flush failure",
            )));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    /// 关闭仍沿用原传输，防止测试控制器持有额外的 writer。
    ///
    /// @author kongweiguang
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[async_trait]
impl SessionTransportFactory for Factory {
    /// 用内存双工流保留实际 reader/writer 路径及预先生成的独立 nonce。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _: &SessionTarget,
        _: TerminalSize,
        _: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let (peer, server) = tokio::io::duplex(65536);
        *self.peer.lock().await = Some(peer);
        let (reader, writer) = tokio::io::split(server);
        let reader = ObservedReader {
            inner: reader,
            gate: self.write_gate.clone(),
        };
        let writer = GatedWriter {
            inner: writer,
            suffix_gate: self.suffix_gate.clone(),
            write_gate: self.write_gate.clone(),
        };
        let transport = if self.conpty {
            SessionTransport::new(reader, writer, ConPtyControl)
        } else {
            SessionTransport::new(reader, writer, Control)
        };
        Ok(transport
            .with_shell_dialect(if self.ssh_powershell {
                RemoteShellDialect::PowerShell
            } else {
                RemoteShellDialect::Posix
            })
            .with_shell_integration_enabled(true)
            .with_shell_editing_nonce(Some(NONCE.to_owned())))
    }
}

/// 声明 Windows ConPTY 能力，使真实 reader 走本地合批路径；其余行为与通用控制一致。
struct ConPtyControl;

struct Control;

#[async_trait]
impl SessionTransportControl for Control {
    /// 测试 transport 的尺寸不依赖原生窗口。
    ///
    /// @author kongweiguang
    async fn resize(&self, _: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 测试控制器不持有 duplex 对端；测试先释放对端，manager 才能确认 reader 已退出。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }

    /// 返回确定退出状态，不把快照成功当成命令执行。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

#[async_trait]
impl SessionTransportControl for ConPtyControl {
    /// 声明本地 ConPTY 合批能力，让 reader 实际走“隐藏后等待重绘”的路径。
    ///
    /// @author kongweiguang
    fn terminal_resize_capability(&self) -> TerminalResizeCapability {
        TerminalResizeCapability::WindowsConPty
    }

    /// 测试 transport 的尺寸不依赖原生窗口。
    ///
    /// @author kongweiguang
    async fn resize(&self, _: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 测试控制器不持有 duplex 对端；测试先释放对端，manager 才能确认 reader 已退出。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }

    /// 返回确定退出状态，不把快照成功当成命令执行。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

/// 创建单次绑定的会话，以小 read chunk 强制跨包解析与日志分流。
///
/// @author kongweiguang
async fn setup() -> (
    Arc<SessionManager>,
    Arc<Recorder>,
    UiSessionBinding,
    DuplexStream,
) {
    setup_with_suffix_gate(None).await
}

/// 仅选定的测试启用后缀背压，其余测试继续使用同一个真实 reader/writer 契约。
///
/// @author kongweiguang
async fn setup_with_suffix_gate(
    suffix_gate: Option<Arc<SuffixGate>>,
) -> (
    Arc<SessionManager>,
    Arc<Recorder>,
    UiSessionBinding,
    DuplexStream,
) {
    setup_with(
        Factory {
            suffix_gate,
            ..Factory::default()
        },
        7,
    )
    .await
}

/// 以 ConPTY 能力和一次性 writer 背压创建真实会话，用于验证 reader 合批的端到端接线。
///
/// @author kongweiguang
async fn setup_redraw_transport(
    ssh_powershell: bool,
    write_gate: Arc<WriteGate>,
) -> (
    Arc<SessionManager>,
    Arc<Recorder>,
    UiSessionBinding,
    DuplexStream,
) {
    setup_with(
        Factory {
            conpty: !ssh_powershell,
            ssh_powershell,
            write_gate: Some(write_gate),
            ..Factory::default()
        },
        4096,
    )
    .await
}

/// 共用会话创建与握手；调用方只决定 transport 能力、背压与单次读取上限。
///
/// @author kongweiguang
async fn setup_with(
    factory: Factory,
    read_chunk_bytes: usize,
) -> (
    Arc<SessionManager>,
    Arc<Recorder>,
    UiSessionBinding,
    DuplexStream,
) {
    let factory = Arc::new(factory);
    let recorder = Arc::new(Recorder::default());
    let config = SessionRuntimeConfig {
        read_chunk_bytes,
        ..SessionRuntimeConfig::default()
    };
    let manager = Arc::new(
        SessionManager::new(factory.clone(), tokio::runtime::Handle::current(), config)
            .with_output_recorder(recorder.clone()),
    );
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let info = manager
        .create(CreateSessionRequest {
            target: if factory.ssh_powershell || factory.ssh_bash {
                SessionTarget::Ssh(SshSessionTarget {
                    host_id: HostId::new(),
                    working_directory: None,
                    command: None,
                })
            } else {
                SessionTarget::Local(LocalSessionTarget {
                    program: OsString::from("test-shell"),
                    args: Vec::new(),
                    working_directory: None,
                    environment: BTreeMap::new(),
                })
            },
            owner: owner.clone(),
            size: TerminalSize::new(80, 24, 0, 0).expect("size"),
            initial_palette: None,
        })
        .await
        .expect("session");
    let binding = UiSessionBinding {
        session_id: info.session_id,
        owner,
        session_generation: 0,
    };
    let mut peer = factory.peer.lock().await.take().expect("peer");
    let startup = format!(
        "visible-start\x1b]133;A\x07\x1b]6977;Kerminal;1;H;{NONCE};bash;1;5.2\x07\x1b]133;B\x07visible-end"
    );
    peer.write_all(startup.as_bytes()).await.expect("startup");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if manager
                .shell_editing_availability(binding.clone())
                .await
                .expect("availability")
                == ShellEditingAvailability::Available
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("handshake");
    (manager, recorder, binding, peer)
}

/// 统一构造协议消息，保证测试输入本身不被当成可信命令或写入历史。
///
/// @author kongweiguang
fn frame(nonce: &str, encoded_input: &str, cursor: usize) -> Vec<u8> {
    format!("\x1b]6977;Kerminal;1;S;{nonce};bash;scalar;{cursor};5.2;/private/path;{encoded_input};0\x07").into_bytes()
}

/// 超限帧不携带正文；Shell 类型和版本仍需与本连接协商结果完全相符。
///
/// @author kongweiguang
fn oversized_refusal(nonce: &str, shell: &str, version: &str) -> Vec<u8> {
    format!("\x1b]6977;Kerminal;1;R;{nonce};{shell};input_too_long;{version}\x07").into_bytes()
}

/// 证明探测只写协商键，不夹带待执行命令或回车。
///
/// @author kongweiguang
async fn expect_probe(peer: &mut DuplexStream) {
    let mut actual = [0; 7];
    tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut actual))
        .await
        .expect("probe timeout")
        .expect("probe read");
    assert_eq!(&actual, PROBE);
}

/// 请求真实快照并由受控 shell 返回带私有正文的分包帧。
///
/// @author kongweiguang
async fn read_snapshot(
    manager: &Arc<SessionManager>,
    binding: &UiSessionBinding,
    peer: &mut DuplexStream,
) -> ShellEditingSnapshot {
    let manager = Arc::clone(manager);
    let binding = binding.clone();
    let request = tokio::spawn(async move {
        manager
            .request_shell_editing_snapshot(binding, CancellationToken::new())
            .await
    });
    expect_probe(peer).await;
    let frame = frame(NONCE, "git%20sta", 7);
    for chunk in frame.chunks(3) {
        peer.write_all(chunk).await.expect("frame chunk");
    }
    request.await.expect("request join").expect("snapshot")
}

/// 快照文本和目录既不进入日志，也不进入外部可见的终端文本；追加只写后缀一次。
///
/// @author kongweiguang
#[tokio::test]
async fn private_frames_are_filtered_and_append_is_confirmed_once() {
    let (manager, recorder, binding, mut peer) = setup().await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    assert_eq!(before.input, "git sta");
    let manager_for_append = Arc::clone(&manager);
    let append_snapshot = before.clone();
    let append = tokio::spawn(async move {
        manager_for_append
            .append_shell_editing_suffix(
                append_snapshot,
                "tus".to_owned(),
                CancellationToken::new(),
            )
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("revalidation");
    let mut suffix = [0; 3];
    tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut suffix))
        .await
        .expect("suffix timeout")
        .expect("suffix");
    assert_eq!(&suffix, b"tus");
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20status", 10))
        .await
        .expect("readback");
    let after = append
        .await
        .expect("append join")
        .expect("confirmed append");
    assert_eq!(after.input, "git status");
    assert_eq!(
        after.context.input_generation,
        before.context.input_generation + 1
    );
    assert!(after.revision > before.revision);
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    let visible = manager
        .plaintext_snapshot(
            binding.session_id.clone(),
            PlainTextSnapshotRequest::default(),
        )
        .await
        .expect("snapshot")
        .text;
    let recorded =
        String::from_utf8(recorder.0.lock().expect("recorder lock").clone()).expect("utf8");
    for output in [&visible, &recorded] {
        assert!(output.contains("visible-start"));
        assert!(!output.contains("git%20sta"));
        assert!(!output.contains("/private/path"));
        assert!(!output.contains(NONCE));
    }
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 真实 ConPTY 合批路径下，隐藏与重绘之间的私有帧不能把首段单独发布：
/// writer 背压时最终快照必须已经包含重绘文本，私有帧不进入日志或可见文本。
///
/// 旧接线会在首段交付后立刻发布并等待 writer；这里在背压点读取快照即可确定性地识别。
///
/// @author kongweiguang
#[tokio::test]
async fn conpty_batch_publishes_redraw_only_after_private_frame() {
    verify_redraw_transport(false).await;
}

/// 即使远端将同一 PowerShell 重绘拆成 SSH 消息，也只发布最终快照。
#[tokio::test]
async fn ssh_powershell_batch_publishes_redraw_only_after_private_frame() {
    verify_redraw_transport(true).await;
}

/// 用同一个交付断言验证两个启用合批的生产入口，避免仅测试策略常量。
async fn verify_redraw_transport(ssh_powershell: bool) {
    let gate = Arc::new(WriteGate::default());
    let (manager, recorder, binding, mut peer) =
        setup_redraw_transport(ssh_powershell, Arc::clone(&gate)).await;
    gate.blocked.store(true, Ordering::Release);
    // 首段携带光标查询以制造响应背压，私有帧、重绘和恢复光标随后到达。
    peer.write_all(HIDE_AND_QUERY).await.expect("hide");
    tokio::time::timeout(Duration::from_secs(2), gate.hide_read.notified())
        .await
        .expect("first transport read");
    let mut redraw = frame(NONCE, "ignored", 0);
    redraw.extend_from_slice(b"\r\nrestored\x1b[?25h");
    peer.write_all(&redraw).await.expect("frame and redraw");
    tokio::time::timeout(Duration::from_secs(2), gate.waiting.notified())
        .await
        .expect("writer backpressure");

    let text = tokio::time::timeout(
        Duration::from_secs(2),
        manager.plaintext_snapshot(
            binding.session_id.clone(),
            PlainTextSnapshotRequest::default(),
        ),
    )
    .await
    .expect("writer backpressure must not hold the snapshot lock")
    .expect("snapshot")
    .text;
    assert!(
        text.contains("restored"),
        "the whole batch must be processed before transport writes; visible text: {text:?}"
    );
    let recorded =
        String::from_utf8(recorder.0.lock().expect("recorder lock").clone()).expect("utf8");
    assert!(recorded.contains("restored"));
    assert!(!recorded.contains(NONCE));

    gate.release();
    let mut response = [0_u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(2), peer.read(&mut response))
        .await
        .expect("cpr response timeout")
        .expect("cpr response");
    assert!(
        read > 0,
        "the pending parser response must still be written"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 让第一段产生终端查询回复来卡住 reader：如果擦除独立发布，背压时快照会只剩空行。
///
/// @author kongweiguang
#[tokio::test]
async fn ssh_bash_probe_keeps_prompt_visible_across_transport_reads() {
    let gate = Arc::new(WriteGate::default());
    let (manager, recorder, binding, mut peer) = setup_with(
        Factory {
            ssh_bash: true,
            write_gate: Some(gate.clone()),
            ..Factory::default()
        },
        4096,
    )
    .await;
    let request = {
        let manager = manager.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            manager
                .request_shell_editing_snapshot(binding, CancellationToken::new())
                .await
        })
    };
    expect_probe(&mut peer).await;
    gate.blocked.store(true, Ordering::Release);
    peer.write_all(BASH_ERASE_AND_QUERY).await.expect("erase");
    tokio::time::timeout(Duration::from_secs(2), gate.hide_read.notified())
        .await
        .expect("erase read independently");
    let mut redraw = frame(NONCE, "a", 1);
    redraw.extend_from_slice(b"testuser@host:$ \x1b]133;B\x07a");
    peer.write_all(&redraw).await.expect("frame and redraw");
    tokio::time::timeout(Duration::from_secs(2), gate.waiting.notified())
        .await
        .expect("writer backpressure");
    let text = tokio::time::timeout(
        Duration::from_secs(2),
        manager.plaintext_snapshot(
            binding.session_id.clone(),
            PlainTextSnapshotRequest::default(),
        ),
    )
    .await
    .expect("snapshot without waiting on the writer")
    .expect("snapshot")
    .text;
    gate.release();
    assert!(text.contains("testuser@host:$ a"), "visible text: {text:?}");
    assert!(
        !recorder
            .0
            .lock()
            .expect("recorder lock")
            .windows(NONCE.len())
            .any(|value| value == NONCE.as_bytes())
    );
    assert_eq!(
        request
            .await
            .expect("request join")
            .expect("editing snapshot")
            .input,
        "a"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 用户取消发生在追加准入之后但首字节之前时，只允许重新读取当前行；不得自动重放原候选。
/// 受控 writer 保证真正经历背压，而不是用调度延迟猜测是否已有后缀送达。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_unwritten_append_recovers_same_prompt_without_replay() {
    let gate = Arc::new(SuffixGate::default());
    gate.blocked.store(true, Ordering::Release);
    let (manager, _recorder, binding, mut peer) =
        setup_with_suffix_gate(Some(Arc::clone(&gate))).await;
    manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: None,
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Text("git sta".to_owned()),
        })
        .await
        .expect("initial UI input");
    let mut typed = [0; 7];
    peer.read_exact(&mut typed).await.expect("initial input");
    assert_eq!(&typed, b"git sta");
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let initial_context = manager
        .terminal_command_context(binding.session_id.clone())
        .await
        .expect("initial context");
    let cancellation = CancellationToken::new();
    let request_cancellation = cancellation.clone();
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before.clone(), "tus".to_owned(), request_cancellation)
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("preflight snapshot");
    tokio::time::timeout(Duration::from_secs(2), gate.waiting.notified())
        .await
        .expect("suffix reaches zero-byte backpressure");
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), append)
        .await
        .expect("cancellation settles")
        .expect("join")
        .expect_err("cancelled suffix is not acknowledged");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let mut unexpected = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut unexpected))
            .await
            .is_err(),
        "zero-byte cancellation must not write or replay the suffix"
    );
    assert_eq!(
        manager
            .shell_editing_availability(binding.clone())
            .await
            .expect("availability"),
        ShellEditingAvailability::Available,
        "the unchanged prompt must permit a new authenticated snapshot"
    );
    assert_eq!(
        manager
            .terminal_command_context(binding.session_id.clone())
            .await
            .expect("unconfirmed projection")
            .availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::UiInputProjectionInvalidated
        ),
        "zero-byte recovery must not restore the old input projection"
    );
    let current = read_snapshot(&manager, &binding, &mut peer).await;
    assert_eq!(current.input, "git sta");
    assert_eq!(current.context.prompt_epoch, initial_context.prompt_epoch);
    assert_eq!(
        current.context.input_generation,
        initial_context.input_generation.wrapping_add(1),
        "input generations remain monotonic even when no bytes were written"
    );
    let current_context = manager
        .terminal_command_context(binding.session_id.clone())
        .await
        .expect("current context");
    assert_eq!(
        current_context.prompt_line_empty,
        initial_context.prompt_line_empty
    );

    // 只有用户重新选择候选后才写入一次；恢复快照本身不复用旧候选。
    gate.blocked.store(false, Ordering::Release);
    let cloned = Arc::clone(&manager);
    let retry = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(current, "tus".to_owned(), CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("retry preflight");
    let mut suffix = [0; 3];
    peer.read_exact(&mut suffix).await.expect("retry suffix");
    assert_eq!(&suffix, b"tus");
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20status", 10))
        .await
        .expect("retry readback");
    assert_eq!(
        retry.await.expect("join").expect("retry").input,
        "git status"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut unexpected))
            .await
            .is_err()
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 被直接丢弃的 future 必须异步清除零字节票据，但不能凭旧候选重发或回退输入代数。
///
/// @author kongweiguang
#[tokio::test]
async fn dropped_unwritten_append_recovers_same_prompt_without_replay() {
    let gate = Arc::new(SuffixGate::default());
    gate.blocked.store(true, Ordering::Release);
    let (manager, _recorder, binding, mut peer) =
        setup_with_suffix_gate(Some(Arc::clone(&gate))).await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let original_context = before.context;
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before, "tus".to_owned(), CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("preflight snapshot");
    tokio::time::timeout(Duration::from_secs(2), gate.waiting.notified())
        .await
        .expect("suffix reaches zero-byte backpressure");
    append.abort();
    assert!(
        append
            .await
            .expect_err("append task aborted")
            .is_cancelled()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if manager
                .shell_editing_availability(binding.clone())
                .await
                .expect("availability")
                == ShellEditingAvailability::Available
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("drop cleanup restores same-prompt snapshot eligibility");
    let mut unexpected = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut unexpected))
            .await
            .is_err(),
        "drop cleanup must not write or replay the suffix"
    );
    let current = read_snapshot(&manager, &binding, &mut peer).await;
    assert_eq!(current.input, "git sta");
    assert_eq!(current.context.prompt_epoch, original_context.prompt_epoch);
    assert_eq!(
        current.context.input_generation,
        original_context.input_generation.wrapping_add(1)
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 部分后缀已被 transport 接受后取消，结果必须保持未知，不能走零字节恢复。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_partially_written_append_remains_unknown() {
    let gate = Arc::new(SuffixGate::default());
    gate.blocked.store(true, Ordering::Release);
    gate.write_prefix.store(true, Ordering::Release);
    let (manager, _recorder, binding, mut peer) =
        setup_with_suffix_gate(Some(Arc::clone(&gate))).await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let cancellation = CancellationToken::new();
    let request_cancellation = cancellation.clone();
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before, "tus".to_owned(), request_cancellation)
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("preflight snapshot");
    tokio::time::timeout(Duration::from_secs(2), gate.waiting.notified())
        .await
        .expect("remaining suffix reaches backpressure");
    let mut prefix = [0; 1];
    peer.read_exact(&mut prefix).await.expect("written prefix");
    assert_eq!(&prefix, b"t");
    cancellation.cancel();
    let error = append.await.expect("join").expect_err("partial suffix");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    assert_eq!(
        manager
            .shell_editing_availability(binding.clone())
            .await
            .expect("availability"),
        ShellEditingAvailability::Unavailable(ShellEditingUnavailableReason::PromptNotReady)
    );
    let mut unexpected = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut unexpected))
            .await
            .is_err(),
        "partial suffix must never be automatically replayed"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 完整后缀已经到达 transport 但 flush 失败，不能把确认失败降级为零字节取消。
///
/// @author kongweiguang
#[tokio::test]
async fn failed_append_flush_remains_unknown() {
    let gate = Arc::new(SuffixGate::default());
    gate.fail_flush.store(true, Ordering::Release);
    let (manager, _recorder, binding, mut peer) =
        setup_with_suffix_gate(Some(Arc::clone(&gate))).await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before, "tus".to_owned(), CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("preflight snapshot");
    let mut suffix = [0; 3];
    peer.read_exact(&mut suffix).await.expect("suffix");
    assert_eq!(&suffix, b"tus");
    let error = append.await.expect("join").expect_err("flush fails");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    assert_eq!(
        manager
            .shell_editing_availability(binding.clone())
            .await
            .expect("availability"),
        ShellEditingAvailability::Unavailable(ShellEditingUnavailableReason::PromptNotReady)
    );
    let mut unexpected = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut unexpected))
            .await
            .is_err(),
        "flush failure must not replay suffix or start readback"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 新提示符取消旧追加后不能用旧行凭据恢复，只有后续真正的新提示符能重置上下文。
///
/// @author kongweiguang
#[tokio::test]
async fn unwritten_append_crossing_prompt_does_not_restore_old_context() {
    let gate = Arc::new(SuffixGate::default());
    gate.blocked.store(true, Ordering::Release);
    let (manager, _recorder, binding, mut peer) =
        setup_with_suffix_gate(Some(Arc::clone(&gate))).await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let original_epoch = before.context.prompt_epoch;
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before, "tus".to_owned(), CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("preflight snapshot");
    tokio::time::timeout(Duration::from_secs(2), gate.waiting.notified())
        .await
        .expect("suffix reaches zero-byte backpressure");
    peer.write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("new prompt cancels old write");
    let error = append.await.expect("join").expect_err("prompt changed");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let new_epoch = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let context = manager
                .terminal_command_context(binding.session_id.clone())
                .await
                .expect("context");
            if context.prompt_epoch > original_epoch && context.prompt_ready {
                assert_eq!(
                    context.availability,
                    TerminalCommandAvailability::Unavailable(
                        TerminalCommandUnavailableReason::ExternalInputObserved
                    )
                );
                break context.prompt_epoch;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("new prompt observed");
    assert_eq!(
        manager
            .shell_editing_availability(binding.clone())
            .await
            .expect("availability"),
        ShellEditingAvailability::Unavailable(ShellEditingUnavailableReason::PromptNotReady)
    );
    let mut unexpected = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut unexpected))
            .await
            .is_err()
    );
    peer.write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("subsequent clean prompt");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let context = manager
                .terminal_command_context(binding.session_id.clone())
                .await
                .expect("context");
            if context.prompt_epoch > new_epoch && context.prompt_ready {
                assert_eq!(context.availability, TerminalCommandAvailability::Available);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("subsequent prompt restores fresh context");
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 错 nonce 的帧不能满足请求，且在相同 reader 顺序中始终不会泄漏正文。
///
/// @author kongweiguang
#[tokio::test]
async fn wrong_nonce_cannot_satisfy_an_outstanding_probe() {
    let (manager, recorder, binding, mut peer) = setup().await;
    let cloned = Arc::clone(&manager);
    let request_binding = binding.clone();
    let request = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(request_binding, CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame("ffffffffffffffffffffffffffffffff", "secret", 6))
        .await
        .expect("wrong nonce");
    peer.write_all(b"processed").await.expect("ordering marker");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let text = manager
                .plaintext_snapshot(
                    binding.session_id.clone(),
                    PlainTextSnapshotRequest::default(),
                )
                .await
                .expect("visible")
                .text;
            if text.contains("processed") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader progress");
    assert!(!request.is_finished());
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("matching nonce");
    assert_eq!(
        request.await.expect("join").expect("snapshot").input,
        "git sta"
    );
    assert!(
        !String::from_utf8_lossy(&recorder.0.lock().expect("recorder lock")).contains("secret")
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// future 被丢弃后仍排空唯一迟到响应，新请求不能在排空前向 Shell 发出探测。
///
/// @author kongweiguang
#[tokio::test]
async fn dropping_probe_drains_late_frame_before_next_request() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let cloned = Arc::clone(&manager);
    let request_binding = binding.clone();
    let request = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(request_binding, CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    request.abort();
    let _ = request.await;
    assert_eq!(
        manager
            .shell_editing_availability(binding.clone())
            .await
            .expect("availability"),
        ShellEditingAvailability::Available
    );
    let next_manager = Arc::clone(&manager);
    let next_binding = binding.clone();
    let next = tokio::spawn(async move {
        next_manager
            .request_shell_editing_snapshot(next_binding, CancellationToken::new())
            .await
    });
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    peer.write_all(&frame(NONCE, "old", 3))
        .await
        .expect("late frame");
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "new", 3))
        .await
        .expect("current frame");
    assert_eq!(next.await.expect("join").expect("snapshot").input, "new");
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 新输入引发协作取消时排空旧帧，后续查询继续使用同一 transport。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_probe_drains_late_frame_and_recovers_for_new_input() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let cancellation = CancellationToken::new();
    let cloned = Arc::clone(&manager);
    let request_binding = binding.clone();
    let request_cancel = cancellation.clone();
    let request = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(request_binding, request_cancel)
            .await
    });
    expect_probe(&mut peer).await;
    cancellation.cancel();
    assert!(request.await.expect("join").is_err());

    let cloned = Arc::clone(&manager);
    let request_binding = binding.clone();
    let next = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(request_binding, CancellationToken::new())
            .await
    });
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    peer.write_all(&frame(NONCE, "stale", 5))
        .await
        .expect("drain old frame");
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "current", 7))
        .await
        .expect("current frame");
    assert_eq!(
        next.await.expect("join").expect("snapshot").input,
        "current"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 已发送但始终无响应的探测超时后必须关闭能力，不能把迟到帧配给新请求。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_probe_without_a_response_eventually_disables_transport() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let cancellation = CancellationToken::new();
    let cloned = Arc::clone(&manager);
    let request_binding = binding.clone();
    let request_cancel = cancellation.clone();
    let request = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(request_binding, request_cancel)
            .await
    });
    expect_probe(&mut peer).await;
    cancellation.cancel();
    assert!(request.await.expect("join").is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if manager
                .shell_editing_availability(binding.clone())
                .await
                .expect("availability")
                == ShellEditingAvailability::Unavailable(
                    ShellEditingUnavailableReason::ProbeTimedOut,
                )
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("abandoned probe timeout");
    assert!(
        manager
            .request_shell_editing_snapshot(binding.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 私有帧超限只关闭增强能力；正文不进入日志，普通终端输入仍然可用。
///
/// @author kongweiguang
#[tokio::test]
async fn oversized_private_frame_disables_only_the_editing_capability() {
    let (manager, recorder, binding, mut peer) = setup().await;
    let mut oversized = SHELL_EDITING_FRAME_PREFIX.to_vec();
    oversized.extend(std::iter::repeat_n(b'x', MAX_SHELL_EDITING_FRAME_BYTES + 1));
    oversized.extend_from_slice(b"\x07after-overflow");
    tokio::time::timeout(Duration::from_secs(10), peer.write_all(&oversized))
        .await
        .expect("oversized frame timeout")
        .expect("oversized frame");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager
                .shell_editing_availability(binding.clone())
                .await
                .expect("availability")
                == ShellEditingAvailability::Unavailable(
                    ShellEditingUnavailableReason::SessionChanged,
                )
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("overflow invalidation");
    assert!(!String::from_utf8_lossy(&recorder.0.lock().expect("recorder lock")).contains("xxxxx"));
    manager
        .write(binding.session_id.clone(), b"x".to_vec())
        .await
        .expect("ordinary input");
    let mut input = [0; 1];
    peer.read_exact(&mut input).await.expect("input byte");
    assert_eq!(&input, b"x");
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 同一 prompt 的 UI 输入变化后消费旧帧，允许下一次探测读取新版本。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_edit_during_probe_discards_old_frame_and_allows_retry() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let cloned = Arc::clone(&manager);
    let request_binding = binding.clone();
    let request = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(request_binding, CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: None,
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Text("x".to_owned()),
        })
        .await
        .expect("UI input");
    let mut input = [0; 1];
    peer.read_exact(&mut input).await.expect("input byte");
    assert_eq!(&input, b"x");
    peer.write_all(&frame(NONCE, "old", 3))
        .await
        .expect("stale frame");
    assert!(request.await.expect("join").is_err());
    let snapshot = read_snapshot(&manager, &binding, &mut peer).await;
    assert_eq!(snapshot.input, "git sta");
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 输入版本变化后旧响应必须消费但不能交付；外部输入仍阻止新探测。
///
/// @author kongweiguang
#[tokio::test]
async fn intervening_input_rejects_snapshot_and_prevents_replay() {
    let (manager, recorder, binding, mut peer) = setup().await;
    let cancel = CancellationToken::new();
    let cloned = Arc::clone(&manager);
    let request_binding = binding.clone();
    let request_cancel = cancel.clone();
    let request = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(request_binding, request_cancel)
            .await
    });
    expect_probe(&mut peer).await;
    manager
        .write(binding.session_id.clone(), b"x".to_vec())
        .await
        .expect("external input");
    let mut input = [0; 1];
    peer.read_exact(&mut input).await.expect("input");
    assert_eq!(&input, b"x");
    peer.write_all(&frame(NONCE, "old%20secret", 10))
        .await
        .expect("stale frame");
    peer.write_all(b"after-stale")
        .await
        .expect("progress marker");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let text = manager
                .plaintext_snapshot(
                    binding.session_id.clone(),
                    PlainTextSnapshotRequest::default(),
                )
                .await
                .expect("visible")
                .text;
            if text.contains("after-stale") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader progress");
    assert!(request.await.expect("join").is_err());
    assert_eq!(
        manager
            .shell_editing_availability(binding.clone())
            .await
            .expect("availability"),
        ShellEditingAvailability::Unavailable(ShellEditingUnavailableReason::PromptNotReady)
    );
    assert!(
        !String::from_utf8_lossy(&recorder.0.lock().expect("recorder lock"))
            .contains("old%20secret")
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// UI 粘贴只失效推测投影，真实快照可在不改变输入代次时恢复后续建议。
///
/// @author kongweiguang
#[tokio::test]
async fn shell_snapshot_recovers_invalidated_ui_projection_without_changing_version() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let invalidated = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: None,
            projection: UiInputProjection::Invalidates,
            event: TerminalInputEvent::Paste("git sta".to_owned()),
        })
        .await
        .expect("UI paste");
    assert_eq!(
        invalidated.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::UiInputProjectionInvalidated
        )
    );
    let mut pasted = [0; 7];
    peer.read_exact(&mut pasted).await.expect("paste bytes");
    assert_eq!(&pasted, b"git sta");
    let snapshot = read_snapshot(&manager, &binding, &mut peer).await;
    let restored = manager
        .terminal_command_context(binding.session_id.clone())
        .await
        .expect("context");
    assert_eq!(snapshot.context, invalidated.version());
    assert_eq!(restored.version(), invalidated.version());
    assert_eq!(
        restored.availability,
        TerminalCommandAvailability::Available
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 已提交 Enter 优先于旧投影失效状态，禁止在命令执行窗口注入探测键。
///
/// @author kongweiguang
#[tokio::test]
async fn submitted_prompt_cannot_be_recovered_from_invalidated_projection() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: None,
            projection: UiInputProjection::Invalidates,
            event: TerminalInputEvent::Paste("x".to_owned()),
        })
        .await
        .expect("paste");
    let submitted = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: None,
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Key(TerminalKeyEvent {
                key: TerminalKey::Enter,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            }),
        })
        .await
        .expect("enter");
    assert_eq!(
        submitted.availability,
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady)
    );
    let mut written = [0; 2];
    peer.read_exact(&mut written).await.expect("input bytes");
    assert_eq!(&written, b"x\r");
    assert!(
        manager
            .request_shell_editing_snapshot(binding.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 追加后目录发生变化时保留已写入事实但报告结果未知，绝不重新发送后缀。
///
/// @author kongweiguang
#[tokio::test]
async fn append_readback_with_changed_directory_is_outcome_unknown() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before, "tus".to_owned(), CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("revalidation");
    let mut suffix = [0; 3];
    peer.read_exact(&mut suffix).await.expect("suffix");
    assert_eq!(&suffix, b"tus");
    expect_probe(&mut peer).await;
    let changed = String::from_utf8(frame(NONCE, "git%20status", 10))
        .expect("frame utf8")
        .replace("/private/path", "/private/changed");
    peer.write_all(changed.as_bytes()).await.expect("readback");
    assert_eq!(
        append
            .await
            .expect("join")
            .expect_err("changed directory")
            .public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 含控制字符或超过任一尺寸上限的后缀在探测和 writer 准入前直接拒绝。
///
/// @author kongweiguang
#[tokio::test]
async fn invalid_suffix_is_rejected_before_any_additional_wire_write() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    for suffix in ["x\n".to_owned(), "😀".repeat(1_025), "x".repeat(3_994)] {
        assert!(
            manager
                .append_shell_editing_suffix(before.clone(), suffix, CancellationToken::new())
                .await
                .is_err()
        );
    }
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 上限使用包含边界的比较：多字节后缀按字节计量，完整输入则按 Unicode 标量计量。
///
/// @author kongweiguang
#[tokio::test]
async fn append_accepts_exact_character_and_byte_limits() {
    for (suffix, encoded_suffix) in [
        ("x".repeat(3_993), "x".repeat(3_993)),
        ("😀".repeat(1_024), "%F0%9F%98%80".repeat(1_024)),
    ] {
        let (manager, _recorder, binding, mut peer) = setup().await;
        let before = read_snapshot(&manager, &binding, &mut peer).await;
        let cloned = Arc::clone(&manager);
        let expected_suffix = suffix.clone();
        let append = tokio::spawn(async move {
            cloned
                .append_shell_editing_suffix(before, suffix, CancellationToken::new())
                .await
        });
        expect_probe(&mut peer).await;
        peer.write_all(&frame(NONCE, "git%20sta", 7))
            .await
            .expect("revalidation");
        let mut written = vec![0; expected_suffix.len()];
        tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut written))
            .await
            .expect("suffix timeout")
            .expect("suffix");
        assert_eq!(written, expected_suffix.as_bytes());
        expect_probe(&mut peer).await;
        peer.write_all(&frame(
            NONCE,
            &format!("git%20sta{encoded_suffix}"),
            7 + expected_suffix.chars().count(),
        ))
        .await
        .expect("readback");
        assert_eq!(
            append.await.expect("join").expect("confirmed append").input,
            format!("git sta{expected_suffix}")
        );
        drop(peer);
        manager.close(binding.session_id).await.expect("close");
    }
}

/// 接近上限的正文仍可读取；超限响应只结束本次请求，缩短后同一连接立即恢复。
///
/// @author kongweiguang
#[tokio::test]
async fn oversized_refusal_recovers_in_same_session_after_shorter_input() {
    let (manager, recorder, binding, mut peer) = setup().await;
    let cloned = Arc::clone(&manager);
    let requested_binding = binding.clone();
    let maximum = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(requested_binding, CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, &"x".repeat(4_000), 4_000))
        .await
        .expect("maximum snapshot");
    assert_eq!(
        maximum.await.expect("join").expect("snapshot").input.len(),
        4_000
    );

    // 4001 与 8001 由 Shell 统一报告超限，不向私有帧重复输出正文。
    for _length in [4_001, 8_001] {
        let cloned = Arc::clone(&manager);
        let requested_binding = binding.clone();
        let request = tokio::spawn(async move {
            cloned
                .request_shell_editing_snapshot(requested_binding, CancellationToken::new())
                .await
        });
        expect_probe(&mut peer).await;
        peer.write_all(&oversized_refusal(NONCE, "bash", "5.2"))
            .await
            .expect("oversized refusal");
        let error = tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .expect("refusal must settle before probe timeout")
            .expect("join")
            .expect_err("oversized input is not a snapshot");
        assert_eq!(
            error.public_code(),
            kerminal_application::PublicErrorCode::Invalid
        );
        assert_eq!(
            manager
                .shell_editing_availability(binding.clone())
                .await
                .expect("availability"),
            ShellEditingAvailability::Available
        );
    }

    assert_eq!(
        read_snapshot(&manager, &binding, &mut peer).await.input,
        "git sta"
    );
    let recorded = String::from_utf8_lossy(&recorder.0.lock().expect("recorder lock")).into_owned();
    assert!(!recorded.contains("input_too_long"));
    assert!(!recorded.contains(&"x".repeat(100)));
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 错 nonce、Shell 或版本的拒绝帧不能抢走当前请求；正确帧仍可返回原始快照。
///
/// @author kongweiguang
#[tokio::test]
async fn forged_oversized_refusals_cannot_settle_pending_probe() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let cloned = Arc::clone(&manager);
    let requested_binding = binding.clone();
    let request = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(requested_binding, CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    for (nonce, shell, version) in [
        ("ffffffffffffffffffffffffffffffff", "bash", "5.2"),
        (NONCE, "zsh", "5.2"),
        (NONCE, "bash", "5.3"),
    ] {
        peer.write_all(&oversized_refusal(nonce, shell, version))
            .await
            .expect("forged refusal");
    }
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("valid frame");
    assert_eq!(
        request.await.expect("join").expect("snapshot").input,
        "git sta"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 接受建议前的重读遇到超限时不写后缀，也不能把未发送误报为未知结果。
///
/// @author kongweiguang
#[tokio::test]
async fn oversized_refusal_before_append_does_not_write_or_poison_session() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before, "tus".to_owned(), CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&oversized_refusal(NONCE, "bash", "5.2"))
        .await
        .expect("oversized refusal");
    let error = tokio::time::timeout(Duration::from_secs(1), append)
        .await
        .expect("refusal must settle before probe timeout")
        .expect("join")
        .expect_err("preflight refused");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    assert_eq!(
        read_snapshot(&manager, &binding, &mut peer).await.input,
        "git sta"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 取消的旧探测仍先排空超限响应，不能将其错误交给新请求或永久占住槽位。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_oversized_refusal_drains_before_next_probe() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let cancellation = CancellationToken::new();
    let cloned = Arc::clone(&manager);
    let requested_binding = binding.clone();
    let request_cancellation = cancellation.clone();
    let old = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(requested_binding, request_cancellation)
            .await
    });
    expect_probe(&mut peer).await;
    cancellation.cancel();
    assert!(old.await.expect("join").is_err());

    let cloned = Arc::clone(&manager);
    let requested_binding = binding.clone();
    let next = tokio::spawn(async move {
        cloned
            .request_shell_editing_snapshot(requested_binding, CancellationToken::new())
            .await
    });
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    peer.write_all(&oversized_refusal(NONCE, "bash", "5.2"))
        .await
        .expect("late oversized refusal");
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("next snapshot");
    assert_eq!(
        next.await.expect("join").expect("snapshot").input,
        "git sta"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}

/// 后缀已写入后回读被拒绝时必须报告未知结果，绝不重放已经送出的字节。
///
/// @author kongweiguang
#[tokio::test]
async fn oversized_refusal_after_append_is_unknown_and_never_replayed() {
    let (manager, _recorder, binding, mut peer) = setup().await;
    let before = read_snapshot(&manager, &binding, &mut peer).await;
    let cloned = Arc::clone(&manager);
    let append = tokio::spawn(async move {
        cloned
            .append_shell_editing_suffix(before, "tus".to_owned(), CancellationToken::new())
            .await
    });
    expect_probe(&mut peer).await;
    peer.write_all(&frame(NONCE, "git%20sta", 7))
        .await
        .expect("preflight snapshot");
    let mut suffix = [0; 3];
    peer.read_exact(&mut suffix).await.expect("suffix");
    assert_eq!(&suffix, b"tus");
    expect_probe(&mut peer).await;
    peer.write_all(&oversized_refusal(NONCE, "bash", "5.2"))
        .await
        .expect("oversized readback");
    let error = tokio::time::timeout(Duration::from_secs(1), append)
        .await
        .expect("refusal must settle before probe timeout")
        .expect("join")
        .expect_err("readback refused");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    let mut extra = [0; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(40), peer.read(&mut extra))
            .await
            .is_err()
    );
    assert_eq!(
        read_snapshot(&manager, &binding, &mut peer).await.input,
        "git sta"
    );
    drop(peer);
    manager.close(binding.session_id).await.expect("close");
}
