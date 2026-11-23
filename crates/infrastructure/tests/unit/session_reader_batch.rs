//! 验证 reader 批次在私有帧两侧只发布一次最终状态，不在隐藏/恢复之间暴露中间重绘。
//!
//! @author kongweiguang

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::AppResult;
use kerminal_application::sessions::{SessionChange, SessionChangeKind, SessionTransportControl};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::identity::{HostId, SessionId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionInfo, SessionLifecycle, SessionOwner,
    SessionTarget, SshSessionTarget, TerminalSize,
};
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
use kerminal_terminal::shell_editing::ShellEditingFrame;
use kerminal_terminal::snapshot::PlainTextSnapshotRequest;
use tokio::io::AsyncWrite;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, broadcast, watch};
use tokio_util::sync::CancellationToken;

use super::{
    AutoReconnectRuntime, ManagedSession, ReaderDeliveryContext, SessionState,
    deliver_reader_batch, editing_runtime, ssh_parser_failure_keeps_reader,
};

const NONCE: &str = "0123456789abcdef0123456789abcdef";

/// 只提供交付路径所需的最小 transport 控制；批次内容本身不触发 close。
struct NoopControl;

#[async_trait]
impl SessionTransportControl for NoopControl {
    /// 测试不依赖原生尺寸，只保证端口有确定结果。
    ///
    /// @author kongweiguang
    async fn resize(&self, _: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 测试可见输出不含关闭请求，因此控制器无需实际关闭资源。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }

    /// 测试会话不等待真实进程退出，返回确定的成功状态。
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

/// 显式 poll 一次并返回结果，便于在 writer 背压处检查已发布 revision。
///
/// @author kongweiguang
async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(|context| Poll::Ready(future.as_mut().poll(context))).await
}

/// 构造只保留交付路径所需字段的会话；engine 从 revision 0 开始，便于观察批次发布次数。
///
/// @author kongweiguang
async fn test_session() -> (
    Arc<ManagedSession>,
    watch::Receiver<SessionChange>,
    TerminalSize,
) {
    let size = TerminalSize::new(80, 24, 0, 0).expect("size");
    let session_id = SessionId::new();
    let engine =
        TerminalEngine::new(session_id.clone(), size, TerminalLimits::default()).expect("engine");
    let (changes, _) = watch::channel(SessionChange {
        revision: 0,
        kind: SessionChangeKind::Lifecycle,
    });
    let mut receiver = changes.subscribe();
    // 提前消费初始值，后续 has_changed 只反映本批次是否发布。
    let _ = receiver.borrow_and_update();
    let (shell_events, _shell_receiver) = broadcast::channel(8);
    let permit = Arc::new(Semaphore::new(1))
        .try_acquire_owned()
        .expect("permit");
    let info = SessionInfo {
        session_id: session_id.clone(),
        target: SessionTarget::Local(LocalSessionTarget {
            program: "test-shell".into(),
            args: Vec::new(),
            working_directory: None,
            environment: Default::default(),
        }),
        owner: SessionOwner::Mcp,
        size,
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    };
    let session = Arc::new(ManagedSession {
        session_id,
        transport_generation: AtomicU64::new(1),
        retiring_transport_generation: Mutex::new(None),
        resize_diagnostic_session_sequence: 1,
        resize_diagnostic_generation: AtomicU64::new(0),
        resize_diagnostic_cpr_generation: AtomicU64::new(0),
        resize_diagnostic_reader_budget: AtomicU64::new(0),
        native_exit_deadline: Mutex::new(None),
        process_id: RwLock::new(None),
        shell_integration_enabled: RwLock::new(false),
        shell_dialect: RwLock::new(RemoteShellDialect::Posix),
        state: Mutex::new(SessionState {
            engine,
            info,
            close_history_proof: None,
        }),
        writer: Mutex::new(Box::pin(tokio::io::sink()) as Pin<Box<dyn AsyncWrite + Send>>),
        writer_shutdown: AtomicBool::new(false),
        control: RwLock::new(Arc::new(NoopControl)),
        cancellation: Mutex::new(CancellationToken::new()),
        changes,
        input_gate: Mutex::new(()),
        zmodem: Mutex::default(),
        shell_integration: Mutex::new(Default::default()),
        shell_editing: Arc::new(StdMutex::new(editing_runtime::EditingRuntime::new(
            1,
            Some(NONCE.to_owned()),
        ))),
        shell_events,
        child_exit_task: Mutex::new(None),
        reader_task: Mutex::new(None),
        output_sequence: AtomicU64::new(0),
        auto_reconnect: Mutex::new(AutoReconnectRuntime::default()),
        auto_reconnect_changed: Notify::new(),
        operation_gate: Mutex::new(()),
        _slot: permit,
    });
    (session, receiver, size)
}

/// 私有帧把一次读取切成两段可见输出时，UI 只能在两段都处理完后收到一次最终 revision；
/// 首段产生的 PTY 响应仍按原顺序等待 transport，不在持锁期间写入。
///
/// 旧实现会在首段交付后立刻发布中间状态，并在这条写路径上等待 writer，因此
/// writer 背压处观察到的 revision 是首段值，本测试会失败。
///
/// @author kongweiguang
#[tokio::test]
async fn private_frame_between_visible_segments_publishes_only_final_state() {
    let (session, mut receiver, _size) = test_session().await;
    // 首段包含光标位置查询，确保旧实现在发布首段变更后确实停在同一 writer 上。
    let visible = b"\x1b[6nleftright";
    let frames = [ShellEditingFrame {
        visible_offset: 8,
        payload: format!("1;H;{NONCE};bash;1;5.2").into_bytes(),
    }];
    let writer = session.writer.lock().await;
    let batch = deliver_reader_batch(
        &session,
        visible,
        &frames,
        ReaderDeliveryContext {
            output_recorder: None,
            control: &NoopControl,
            transport_generation: 1,
            close_wait: Duration::from_secs(1),
            editing_invalidated: false,
            write_responses: true,
        },
    );
    tokio::pin!(batch);
    assert!(
        poll_once(batch.as_mut()).await.is_pending(),
        "the batch must wait for the transport writer"
    );
    assert!(
        receiver.has_changed().expect("change sender alive"),
        "the completed batch must publish its final state"
    );
    assert_eq!(
        receiver.borrow_and_update().revision,
        2,
        "only the final segment revision may be published; an intermediate revision means \
         hidden output was exposed between a private frame"
    );
    assert_eq!(
        session
            .output_sequence
            .load(std::sync::atomic::Ordering::Acquire),
        2,
        "both visible segments must be recorded before the transport write"
    );
    drop(writer);
    batch.await.expect("batch delivery");
}

/// 只改变标题的批次保留既有 Title 语义，避免升级所有标题通知。
///
/// @author kongweiguang
#[tokio::test]
async fn pure_title_batch_keeps_title_kind() {
    let (session, mut receiver, _size) = test_session().await;
    deliver_reader_batch(
        &session,
        b"\x1b]2;hello\x07",
        &[],
        ReaderDeliveryContext {
            output_recorder: None,
            control: &NoopControl,
            transport_generation: 1,
            close_wait: Duration::from_secs(1),
            editing_invalidated: false,
            write_responses: true,
        },
    )
    .await
    .expect("title batch");
    let change = *receiver.borrow_and_update();
    assert_eq!(change.kind, SessionChangeKind::Title);
}

/// 标题与 Shell 事件同批到达时必须发 Output，否则消费者会漏掉 prompt 状态推进。
///
/// @author kongweiguang
#[tokio::test]
async fn title_with_output_events_publishes_output_kind() {
    let (session, mut receiver, _size) = test_session().await;
    deliver_reader_batch(
        &session,
        b"\x1b]2;hello\x07\x1b]133;A\x07",
        &[],
        ReaderDeliveryContext {
            output_recorder: None,
            control: &NoopControl,
            transport_generation: 1,
            close_wait: Duration::from_secs(1),
            editing_invalidated: false,
            write_responses: true,
        },
    )
    .await
    .expect("title and marker batch");
    let change = *receiver.borrow_and_update();
    assert_eq!(change.kind, SessionChangeKind::Output);
}

/// 仅私有帧的批次超限时没有可见字节，也必须复用批次变更发布 Output 让编辑能力失效可见。
///
/// @author kongweiguang
#[tokio::test]
async fn private_frame_overflow_publishes_output_kind() {
    let (session, mut receiver, _size) = test_session().await;
    deliver_reader_batch(
        &session,
        &[],
        &[],
        ReaderDeliveryContext {
            output_recorder: None,
            control: &NoopControl,
            transport_generation: 1,
            close_wait: Duration::from_secs(1),
            editing_invalidated: true,
            write_responses: true,
        },
    )
    .await
    .expect("overflow batch");
    let change = *receiver.borrow_and_update();
    assert_eq!(change.kind, SessionChangeKind::Output);
}

/// 确认窗口内仍完成解析和通知，只跳过已失败 writer 的响应；后续可见字节不得丢失。
///
/// @author kongweiguang
#[tokio::test]
async fn suppressed_parser_responses_still_deliver_output() {
    let (session, receiver, _size) = test_session().await;
    let writer = session.writer.lock().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        deliver_reader_batch(
            &session,
            b"\x1b[6nafter-failure",
            &[],
            ReaderDeliveryContext {
                output_recorder: None,
                control: &NoopControl,
                transport_generation: 1,
                close_wait: Duration::from_secs(1),
                editing_invalidated: false,
                write_responses: false,
            },
        ),
    )
    .await
    .expect("delivery must not wait for the failed transport writer")
    .expect("batch delivery");
    assert!(receiver.has_changed().expect("change sender alive"));
    let text = session
        .state
        .lock()
        .await
        .engine
        .plaintext_snapshot(PlainTextSnapshotRequest::default())
        .text;
    assert!(text.contains("after-failure"));
    drop(writer);
}

/// 编辑帧被接受时即使同批只有标题变化也必须发 Output，避免 UI 只刷新标题。
///
/// @author kongweiguang
#[tokio::test]
async fn accepted_editing_frame_with_title_publishes_output_kind() {
    let (session, mut receiver, _size) = test_session().await;
    let frames = [ShellEditingFrame {
        visible_offset: 0,
        payload: format!("1;H;{NONCE};bash;1;5.2").into_bytes(),
    }];
    deliver_reader_batch(
        &session,
        b"\x1b]2;hello\x07",
        &frames,
        ReaderDeliveryContext {
            output_recorder: None,
            control: &NoopControl,
            transport_generation: 1,
            close_wait: Duration::from_secs(1),
            editing_invalidated: false,
            write_responses: true,
        },
    )
    .await
    .expect("editing and title batch");
    let change = *receiver.borrow_and_update();
    assert_eq!(change.kind, SessionChangeKind::Output);
}

/// 回写失败的确认窗口只对当前代、Running 的 SSH 会话开放；旧代与主动关闭立即收口，
/// 避免把旧 reader 的回执或正在关闭的会话误判为可重试断线。
///
/// @author kongweiguang
#[tokio::test]
async fn parser_write_failure_guard_requires_current_running_ssh_generation() {
    let (session, _receiver, _size) = test_session().await;
    session.state.lock().await.info.target = SessionTarget::Ssh(SshSessionTarget {
        host_id: HostId::new(),
        working_directory: None,
        command: None,
    });
    assert!(ssh_parser_failure_keeps_reader(&session, 1).await);
    // 旧 generation 的回写失败不得延长确认窗口。
    assert!(!ssh_parser_failure_keeps_reader(&session, 0).await);
    *session.retiring_transport_generation.lock().await = Some(1);
    assert!(!ssh_parser_failure_keeps_reader(&session, 1).await);
    *session.retiring_transport_generation.lock().await = None;
    // 主动关闭或已结束的会话不再等待读泵确认。
    session.state.lock().await.info.lifecycle = SessionLifecycle::Closing;
    assert!(!ssh_parser_failure_keeps_reader(&session, 1).await);
    session.state.lock().await.info.lifecycle = SessionLifecycle::Running;
    // 非 SSH transport 仍走立即失败路径。
    let (local, _local_receiver, _local_size) = test_session().await;
    assert!(!ssh_parser_failure_keeps_reader(&local, 1).await);
}
