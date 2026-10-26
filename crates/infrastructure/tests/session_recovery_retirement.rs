//! 退役中的旧 transport 错误不能把 Starting 污染成新代次失败。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    sessions::{
        CreateSessionRequest, RenderSnapshotRequest, ScrollRequest, SessionService,
        SessionTransport, SessionTransportControl, SessionTransportFactory,
        SessionTransportOpenError, TerminalSearchRequest, UiSessionBinding,
        UiTerminalSearchRequest,
    },
};
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{PaneId, TabId},
    sessions::{
        LocalSessionTarget, SessionExitStatus, SessionInfo, SessionLifecycle, SessionOwner,
        SessionTarget, TerminalSize,
    },
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use kerminal_terminal::snapshot::{TerminalPoint, TerminalSelection, TerminalSelectionKind};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::{Mutex, Notify, oneshot},
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InjectedFailure {
    Reader,
    ParserResponseWrite,
}

/// 阻塞首代 close ACK，保证错误注入发生在 Starting 已发布、旧 reader 尚未 join 的窗口。
struct RetirementBarrierControl {
    close_calls: AtomicUsize,
    close_entered: Arc<Notify>,
    close_release: Arc<Notify>,
    fail_first_close: bool,
    peer: Arc<Mutex<Option<DuplexStream>>>,
}

impl RetirementBarrierControl {
    /// 为旧 owner 保存独立 close 计数与 reader peer，失败时让相同资源可再次收尾。
    ///
    /// @author kongweiguang
    fn new(
        close_entered: Arc<Notify>,
        close_release: Arc<Notify>,
        fail_first_close: bool,
        peer: Arc<Mutex<Option<DuplexStream>>>,
    ) -> Self {
        Self {
            close_calls: AtomicUsize::new(0),
            close_entered,
            close_release,
            fail_first_close,
            peer,
        }
    }
}

#[async_trait]
impl SessionTransportControl for RetirementBarrierControl {
    /// 保持 fake transport 不依赖原生 PTY 尺寸，只让 manager 的退役门禁决定顺序。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 在首次 close 暂停确认；失败时不释放 peer，供恢复路径重试同一 owner。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        let call = self.close_calls.fetch_add(1, Ordering::AcqRel);
        if call == 0 {
            self.close_entered.notify_one();
            self.close_release.notified().await;
            if self.fail_first_close {
                return Err(AppError::infrastructure(
                    "test old transport close fails before retirement ACK",
                ));
            }
        }
        self.peer.lock().await.take();
        Ok(())
    }

    /// fake control 在确认 close 后立即返回退出状态，避免测试被 child watcher 时序主导。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(successful_exit_status())
    }
}

/// 新 transport 的 close 只丢弃 duplex peer，使 manager 的真实 reader 收到 EOF 并可 join。
struct PeerControl {
    peer: Mutex<Option<DuplexStream>>,
}

#[async_trait]
impl SessionTransportControl for PeerControl {
    /// 新代次使用同尺寸 fake control，无需引入 native resize 副作用。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// close ACK 同时结束 reader peer，避免测试留存受管 Tokio task。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.peer.lock().await.take();
        Ok(())
    }

    /// peer 被 close 后无需等待外部 child，返回固定的成功状态。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(successful_exit_status())
    }
}

/// 在测试触发后让旧 reader 只返回一次 I/O 错误，不用 sleep 猜测 PTY 调度顺序。
struct ErrorOnSignalReader {
    failure: oneshot::Receiver<()>,
}

impl AsyncRead for ErrorOnSignalReader {
    /// 以 oneshot 作为显式调度屏障，让用例在旧代退役开始后精确注入 read Err。
    ///
    /// @author kongweiguang
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        _buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.get_mut().failure).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => {
                Poll::Ready(Err(io::Error::other("test old transport reader failure")))
            }
        }
    }
}

/// 只在 parser 生成 PTY response 时失败，记录调用次数证明测试确实经过 writer 失败支路。
struct FailingResponseWriter {
    write_attempts: Arc<AtomicUsize>,
}

impl AsyncWrite for FailingResponseWriter {
    /// parser response 的首次写入立即失败，外部命令输入和数据并非本用例的触发路径。
    ///
    /// @author kongweiguang
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.write_attempts.fetch_add(1, Ordering::AcqRel);
        Poll::Ready(Err(io::Error::other("test parser response write failure")))
    }

    /// 无缓冲 writer 的 flush 不增加与 parser response 写失败无关的调度因素。
    ///
    /// @author kongweiguang
    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// retirement 会先 shutdown writer；保留 writer 对象使后续 parser response 仍能走失败回执路径。
    ///
    /// @author kongweiguang
    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// 把故障限制在首代 transport，后续代可由 manager 正常关闭与 join。
struct RecoveryFactory {
    opens: AtomicUsize,
    failure: InjectedFailure,
    close_entered: Arc<Notify>,
    close_release: Arc<Notify>,
    old_control: Arc<RetirementBarrierControl>,
    old_peer: Arc<Mutex<Option<DuplexStream>>>,
    reader_failure: Mutex<Option<oneshot::Sender<()>>>,
    writer_attempts: Arc<AtomicUsize>,
}

impl RecoveryFactory {
    /// 为第一代独立安装可控 reader、parser writer 与 close barrier，后续代使用可正常回收的 peer。
    ///
    /// @author kongweiguang
    fn new(failure: InjectedFailure, fail_first_close: bool) -> Self {
        let close_entered = Arc::new(Notify::new());
        let close_release = Arc::new(Notify::new());
        let old_peer = Arc::new(Mutex::new(None));
        Self {
            opens: AtomicUsize::new(0),
            failure,
            close_entered: Arc::clone(&close_entered),
            close_release: Arc::clone(&close_release),
            old_control: Arc::new(RetirementBarrierControl::new(
                close_entered,
                close_release,
                fail_first_close,
                Arc::clone(&old_peer),
            )),
            old_peer,
            reader_failure: Mutex::new(None),
            writer_attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 在 close barrier 已进入后注入指定的旧 transport 故障，不向日志输出终端正文。
    ///
    /// @author kongweiguang
    async fn inject_failure(&self) {
        match self.failure {
            InjectedFailure::Reader => {
                self.reader_failure
                    .lock()
                    .await
                    .take()
                    .expect("first transport installed its reader failure gate")
                    .send(())
                    .expect("old reader is still awaiting the test signal");
            }
            InjectedFailure::ParserResponseWrite => {
                self.old_peer
                    .lock()
                    .await
                    .as_mut()
                    .expect("first transport keeps its reader peer open")
                    .write_all(b"\x1b[6n")
                    .await
                    .expect("write fixed device-status query to old reader");
            }
        }
    }
}

#[async_trait]
impl SessionTransportFactory for RecoveryFactory {
    /// 第一代按用例返回 read-error 或 parser-response-error transport，后续 open 返回可正常关闭的 reader。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test transport open cancelled").into());
        }
        if self.opens.fetch_add(1, Ordering::AcqRel) == 0 {
            let reader: Pin<Box<dyn AsyncRead + Send>> = match self.failure {
                InjectedFailure::Reader => {
                    let (signal, failure) = oneshot::channel();
                    *self.reader_failure.lock().await = Some(signal);
                    Box::pin(ErrorOnSignalReader { failure })
                }
                InjectedFailure::ParserResponseWrite => {
                    let (reader, peer) = tokio::io::duplex(256);
                    *self.old_peer.lock().await = Some(peer);
                    Box::pin(reader)
                }
            };
            let writer: Pin<Box<dyn AsyncWrite + Send>> = Box::pin(FailingResponseWriter {
                write_attempts: Arc::clone(&self.writer_attempts),
            });
            return Ok(SessionTransport {
                reader,
                writer,
                control: self.old_control.clone(),
                shell_dialect: RemoteShellDialect::Unknown,
                process_id: None,
                shell_integration_enabled: false,
            });
        }

        let (reader, peer) = tokio::io::duplex(256);
        Ok(SessionTransport::new(
            reader,
            tokio::io::sink(),
            PeerControl {
                peer: Mutex::new(Some(peer)),
            },
        ))
    }
}

/// 统一固定退出结果，避免 fake transport 通过 native child 状态制造额外状态变化。
///
/// @author kongweiguang
fn successful_exit_status() -> SessionExitStatus {
    SessionExitStatus {
        code: Some(0),
        signal: None,
        success: true,
    }
}

/// 创建拥有本地假 shell 的真实 SessionManager session，不访问用户 shell 或外部主机。
///
/// @author kongweiguang
async fn create_session(factory: Arc<RecoveryFactory>) -> (Arc<SessionManager>, SessionInfo) {
    let manager = Arc::new(SessionManager::new(
        factory,
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            close_wait: Duration::from_secs(2),
            ..SessionRuntimeConfig::default()
        },
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("retirement-test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
            initial_palette: None,
        })
        .await
        .expect("create first fake transport");
    (manager, info)
}

/// 在旧 control 的 close ACK 被阻塞时注入失败，使 reader join 必须先处理退役回执再发布新代次。
///
/// @author kongweiguang
async fn exercise_retiring_failure(failure: InjectedFailure, fail_first_close: bool) {
    let factory = Arc::new(RecoveryFactory::new(failure, fail_first_close));
    let (manager, created) = create_session(Arc::clone(&factory)).await;
    let close_entered = Arc::clone(&factory.close_entered);
    let close_release = Arc::clone(&factory.close_release);
    let close_started = close_entered.notified();
    let restart_manager = Arc::clone(&manager);
    let session_id = created.session_id.clone();
    let restart = tokio::spawn(async move {
        restart_manager
            .restart_cancellable(session_id, CancellationToken::new())
            .await
    });

    timeout(Duration::from_secs(2), close_started)
        .await
        .expect("restart reached the old transport close barrier");
    let during_retirement = manager
        .list(Default::default())
        .await
        .expect("list session while old close is blocked")
        .into_iter()
        .find(|info| info.session_id == created.session_id)
        .expect("session remains owned during retirement");
    assert_eq!(during_retirement.lifecycle, SessionLifecycle::Starting);

    factory.inject_failure().await;
    close_release.notify_one();
    let first_result = timeout(Duration::from_secs(3), restart)
        .await
        .expect("restart finishes after close release")
        .expect("manager-owned restart worker remains joinable");

    let recovered = if fail_first_close {
        assert!(
            first_result.is_err(),
            "first close ACK must fail in this case"
        );
        assert_eq!(factory.opens.load(Ordering::Acquire), 1);
        let failed_owner = manager
            .list(Default::default())
            .await
            .expect("list retained session after close failure")
            .into_iter()
            .find(|info| info.session_id == created.session_id)
            .expect("failed close keeps the original registry owner");
        assert_eq!(failed_owner.lifecycle, SessionLifecycle::Failed);
        timeout(
            Duration::from_secs(3),
            manager.restart_cancellable(created.session_id.clone(), CancellationToken::new()),
        )
        .await
        .expect("retry of the retained old owner finishes")
        .expect("same old control can complete close on retry")
    } else {
        first_result.expect("retiring old failure must not poison the replacement transport")
    };

    assert_eq!(recovered.session_id, created.session_id);
    assert_eq!(recovered.lifecycle, SessionLifecycle::Running);
    assert_eq!(factory.opens.load(Ordering::Acquire), 2);
    if failure == InjectedFailure::ParserResponseWrite {
        assert!(
            factory.writer_attempts.load(Ordering::Acquire) > 0,
            "fixed device-status query must reach the failing parser-response writer"
        );
    }
    manager
        .close(created.session_id)
        .await
        .expect("close replacement transport and join its reader");
}

/// Read Err during Starting must be ignored only for the old retiring generation.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn old_reader_error_during_restart_does_not_fail_new_generation() {
    exercise_retiring_failure(InjectedFailure::Reader, false).await;
}

/// A VT parser response write error from the old writer must not invalidate the replacement transport.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn old_parser_response_write_error_during_restart_is_generation_scoped() {
    exercise_retiring_failure(InjectedFailure::ParserResponseWrite, false).await;
}

/// A failed close retains the same old owner and retirement marker so a later explicit restart can retry it.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn failed_retirement_close_keeps_old_owner_retryable_after_reader_error() {
    exercise_retiring_failure(InjectedFailure::Reader, true).await;
}

/// A close failure restores only the ended UI history surface while keeping writer and transport resize closed.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn failed_close_from_failed_lifecycle_retains_read_only_history_until_retry() {
    let factory = Arc::new(RecoveryFactory::new(InjectedFailure::Reader, true));
    let (manager, created) = create_session(Arc::clone(&factory)).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("capture the original UI generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner: created.owner.clone(),
        session_generation: context.session_generation,
    };
    factory.inject_failure().await;
    let failed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let info = manager
                .list(Default::default())
                .await
                .expect("observe reader failure")
                .into_iter()
                .find(|info| info.session_id == created.session_id)
                .expect("failed session keeps its registry owner");
            if info.lifecycle == SessionLifecycle::Failed {
                break info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader failure transitions the old lifecycle before close");
    assert_eq!(failed.lifecycle, SessionLifecycle::Failed);

    let close_entered = Arc::clone(&factory.close_entered);
    let close_release = Arc::clone(&factory.close_release);
    let close_started = close_entered.notified();
    let close_manager = Arc::clone(&manager);
    let close_session_id = created.session_id.clone();
    let close_task = tokio::spawn(async move {
        close_manager
            .close_until(close_session_id, Instant::now() + Duration::from_secs(4))
            .await
    });
    timeout(Duration::from_secs(2), close_started)
        .await
        .expect("close reaches the first transport ACK barrier");
    close_release.notify_one();
    assert!(
        timeout(Duration::from_secs(3), close_task)
            .await
            .expect("failed close returns after the barrier release")
            .expect("close task remains joinable")
            .is_err()
    );

    let after_failure = manager
        .list(Default::default())
        .await
        .expect("list retained old owner")
        .into_iter()
        .find(|info| info.session_id == created.session_id)
        .expect("failed close keeps the owner for explicit retry");
    assert_eq!(after_failure.lifecycle, SessionLifecycle::Failed);
    manager
        .scroll_ui(binding.clone(), ScrollRequest { lines: 1 })
        .await
        .expect("ended history scroll remains available after close failure");
    manager
        .search_ui_cancellable(
            binding.clone(),
            UiTerminalSearchRequest {
                request: TerminalSearchRequest {
                    query: "absent".to_owned(),
                    case_sensitive: false,
                    regex: false,
                    backwards: false,
                    max_matches: 8,
                },
                selected_match: None,
                viewport:
                    kerminal_application::sessions::UiTerminalSearchViewport::PreserveViewport,
                commit_gate: kerminal_application::sessions::UiTerminalSearchCommitGate::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("ended history search remains available after close failure");
    manager
        .select_all_ui(binding.clone())
        .await
        .expect("ended history selection remains available after close failure");
    manager
        .selected_text_ui(binding.clone())
        .await
        .expect("history copy validates the same owner and generation");
    let history_grid = TerminalSize::new(79, 24, 0, 0).expect("valid local view grid");
    manager
        .resize_ui_view(binding, history_grid)
        .await
        .expect("ended history may reflow without touching transport");
    assert!(
        manager
            .write(created.session_id.clone(), b"must remain closed".to_vec())
            .await
            .is_err(),
        "history restoration must never reopen writer access"
    );
    assert!(
        manager
            .resize(
                created.session_id.clone(),
                TerminalSize::new(80, 25, 0, 0).expect("valid transport grid"),
            )
            .await
            .is_err(),
        "history resize must not authorize transport resize"
    );

    manager
        .close(created.session_id.clone())
        .await
        .expect("retry closes the same retained transport owner");
    assert!(
        manager
            .list(Default::default())
            .await
            .expect("list after successful retry")
            .is_empty()
    );
}

/// A delayed selection from the retired view must not clear a new generation's current selection.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn stale_ui_selection_binding_cannot_mutate_reconnected_generation() {
    let factory = Arc::new(RecoveryFactory::new(InjectedFailure::Reader, false));
    let (manager, created) = create_session(Arc::clone(&factory)).await;
    let old_context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read initial runtime generation");
    let stale_binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner: created.owner.clone(),
        session_generation: old_context.session_generation,
    };

    let close_entered = Arc::clone(&factory.close_entered);
    let close_release = Arc::clone(&factory.close_release);
    let close_started = close_entered.notified();
    let restart_manager = Arc::clone(&manager);
    let restart_session = created.session_id.clone();
    let restart = tokio::spawn(async move {
        restart_manager
            .restart_cancellable(restart_session, CancellationToken::new())
            .await
    });
    timeout(Duration::from_secs(2), close_started)
        .await
        .expect("restart reached the old transport close barrier");
    factory.inject_failure().await;
    close_release.notify_one();
    timeout(Duration::from_secs(3), restart)
        .await
        .expect("restart completes after the retirement barrier is released")
        .expect("manager-owned restart remains joinable")
        .expect("restart and publish a new transport generation");
    let new_context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read replacement runtime generation");
    assert_ne!(
        stale_binding.session_generation, new_context.session_generation,
        "reconnect must invalidate the old view binding"
    );
    let current_binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner: created.owner,
        session_generation: new_context.session_generation,
    };
    let current_selection = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 1 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection(current_binding.clone(), Some(current_selection), Some(77))
        .await
        .expect("start selection against the current visible generation");
    let drag_update = TerminalSelection {
        start: TerminalPoint { line: 1, column: 1 },
        end: TerminalPoint { line: 0, column: 3 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection(current_binding, Some(drag_update), Some(77))
        .await
        .expect("update the same gesture without replacing its original anchor");

    let before_stale = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read current selection before delayed event");
    let anchored_selection = TerminalSelection {
        start: current_selection.start,
        end: drag_update.end,
        kind: current_selection.kind,
    };
    assert_eq!(before_stale.selection, Some(anchored_selection));

    assert!(
        manager
            .set_ui_selection(stale_binding, None, Some(u64::MAX))
            .await
            .is_err()
    );
    let after_stale = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read selection after rejecting delayed event");
    assert_eq!(
        after_stale.selection,
        Some(anchored_selection),
        "stale view cancellation must not clear a new generation's selection"
    );

    manager
        .close(created.session_id)
        .await
        .expect("close replacement transport and join its reader");
}
