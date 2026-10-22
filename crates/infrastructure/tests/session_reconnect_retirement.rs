//! 用真实 SessionManager 验证旧 transport 退役失败后的所有权、状态与重试顺序。
//!
//! @author kongweiguang

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    sessions::{
        CreateSessionRequest, SessionService, SessionTransport, SessionTransportControl,
        SessionTransportFactory, SessionTransportOpenError,
    },
};
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{HostId, PaneId, TabId},
    sessions::{
        SessionExitStatus, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        SshSessionTarget, TerminalSize,
    },
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::{Mutex, Notify},
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetirementBehavior {
    Success,
    CloseFailsOnce,
    Running,
    RunningCloseFailsOnce,
    RunningCloseWaitsForRelease,
    WaitFailsOnce,
    WaitTimesOutOnce,
    WriterShutdownFailsOnce,
    TypedDisconnect,
    TypedDisconnectPersistentBrokenPipe,
    TypedDisconnectBrokenPipeCloseFailsOnce,
    PersistentWriterFailure,
}

struct RetirementControl {
    behavior: RetirementBehavior,
    close_calls: AtomicUsize,
    wait_calls: AtomicUsize,
    close_acknowledged: AtomicBool,
    exit_failure_enabled: AtomicBool,
    peer: Mutex<Option<DuplexStream>>,
    close_entered: Option<Arc<Notify>>,
    close_release: Option<Arc<Notify>>,
}

impl RetirementControl {
    /// 每个 transport 独立持有调用计数与 reader 对端，使重试不能被新 control 的成功掩盖。
    ///
    /// @author kongweiguang
    fn new(behavior: RetirementBehavior, peer: Option<DuplexStream>) -> Self {
        Self {
            behavior,
            close_calls: AtomicUsize::new(0),
            wait_calls: AtomicUsize::new(0),
            close_acknowledged: AtomicBool::new(false),
            exit_failure_enabled: AtomicBool::new(true),
            peer: Mutex::new(peer),
            close_entered: None,
            close_release: None,
        }
    }

    /// 仅给 abort barrier 测试的首代 control 安装可控 close 门，稳定制造 Starting 窗口。
    ///
    /// @author kongweiguang
    fn with_close_barrier(
        behavior: RetirementBehavior,
        peer: Option<DuplexStream>,
        close_entered: Arc<Notify>,
        close_release: Arc<Notify>,
    ) -> Self {
        Self {
            behavior,
            close_calls: AtomicUsize::new(0),
            wait_calls: AtomicUsize::new(0),
            close_acknowledged: AtomicBool::new(false),
            exit_failure_enabled: AtomicBool::new(true),
            peer: Mutex::new(peer),
            close_entered: Some(close_entered),
            close_release: Some(close_release),
        }
    }
}

#[async_trait]
impl SessionTransportControl for RetirementControl {
    /// 不引入实际 PTY 几何，把断言集中在 manager 的资源发布和退役顺序。
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 故意让旧资源失败一次；只有成功 ACK 才关闭 reader 对端，供同一 owner 再次收尾。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        let call = self.close_calls.fetch_add(1, Ordering::AcqRel);
        if self.behavior == RetirementBehavior::RunningCloseWaitsForRelease && call == 0 {
            self.close_entered
                .as_ref()
                .expect("barrier close has an entered signal")
                .notify_one();
            self.close_release
                .as_ref()
                .expect("barrier close has a release signal")
                .notified()
                .await;
        }
        if matches!(
            self.behavior,
            RetirementBehavior::CloseFailsOnce
                | RetirementBehavior::RunningCloseFailsOnce
                | RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce
        ) && call == 0
        {
            return Err(AppError::infrastructure(
                "test old transport close fails once",
            ));
        }
        self.peer.lock().await.take();
        self.close_acknowledged.store(true, Ordering::Release);
        Ok(())
    }

    /// 固定普通未知、悬挂与 typed disconnect 结果，避免测试依赖外部 shell 或网络时序。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        let call = self.wait_calls.fetch_add(1, Ordering::AcqRel);
        match self.behavior {
            RetirementBehavior::WaitFailsOnce if call == 0 => {
                return Err(AppError::infrastructure(
                    "test old transport wait fails once",
                ));
            }
            RetirementBehavior::WaitTimesOutOnce if call == 0 => {
                return std::future::pending().await;
            }
            RetirementBehavior::TypedDisconnect
            | RetirementBehavior::TypedDisconnectPersistentBrokenPipe => {
                return Err(AppError::retryable_remote_disconnect(io::Error::other(
                    "test remote exit status is unavailable",
                )));
            }
            RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce => {
                return Err(AppError::retryable_remote_disconnect(io::Error::other(
                    "test remote exit status is unavailable",
                )));
            }
            RetirementBehavior::PersistentWriterFailure
                if self.exit_failure_enabled.load(Ordering::Acquire) =>
            {
                return Err(AppError::infrastructure(
                    "test old transport exit is unknown",
                ));
            }
            _ => {}
        }
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

struct FailedReader;

impl AsyncRead for FailedReader {
    /// 让初始 session 真实经过 reader failure 进入 Failed，避免 EOF 预先替测试关闭 writer。
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other("test initial reader disconnected")))
    }
}

struct RemoteCloseReader {
    inner: DuplexStream,
    remote_channel_end_confirmed: Arc<AtomicBool>,
}

impl AsyncRead for RemoteCloseReader {
    /// 仅在同一 reader 真实观察到 EOF 时公布 channel end，模拟 SSH adapter 的精确 proof 归属。
    ///
    /// @author kongweiguang
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buffer);
        if matches!(&result, Poll::Ready(Ok(()))) && buffer.filled().len() == filled_before {
            this.remote_channel_end_confirmed
                .store(true, Ordering::Release);
        }
        result
    }
}

struct RetirementWriter {
    shutdown_calls: Arc<AtomicUsize>,
    write_calls: Arc<AtomicUsize>,
    written_bytes: Arc<AtomicUsize>,
    fail_first_shutdown: bool,
    persistent_shutdown_error: Option<io::ErrorKind>,
    persistent_failure_enabled: Arc<AtomicBool>,
    remote_channel_end_confirmed: Option<Arc<AtomicBool>>,
}

impl AsyncWrite for RetirementWriter {
    /// 写入本身不制造失败，确保被测错误只来自重连前的 writer 退役阶段。
    ///
    /// @author kongweiguang
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.write_calls.fetch_add(1, Ordering::AcqRel);
        self.written_bytes.fetch_add(bytes.len(), Ordering::AcqRel);
        Poll::Ready(Ok(bytes.len()))
    }

    /// 测试 writer 不缓存输出，不为 flush 添加与退役无关的调度因素。
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// 仅带 reader 的 channel-end proof 时跳过不可达 shutdown；无 proof 仍注入故障并保留 owner。
    ///
    /// @author kongweiguang
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self
            .remote_channel_end_confirmed
            .as_ref()
            .is_some_and(|proof| proof.load(Ordering::Acquire))
        {
            return Poll::Ready(Ok(()));
        }
        let call = self.shutdown_calls.fetch_add(1, Ordering::AcqRel);
        if self.persistent_failure_enabled.load(Ordering::Acquire)
            && let Some(kind) = self.persistent_shutdown_error
        {
            return Poll::Ready(Err(io::Error::new(
                kind,
                "test persistent writer shutdown failure",
            )));
        }
        if self.fail_first_shutdown && call == 0 {
            return Poll::Ready(Err(io::Error::other("test writer shutdown fails once")));
        }
        Poll::Ready(Ok(()))
    }
}

struct RetirementFactory {
    opens: AtomicUsize,
    old_control: Arc<RetirementControl>,
    old_writer_shutdown_calls: Arc<AtomicUsize>,
    old_writer_write_calls: Arc<AtomicUsize>,
    old_writer_written_bytes: Arc<AtomicUsize>,
    old_remote_channel_end_confirmed: Arc<AtomicBool>,
    replacement_writer_write_calls: Arc<AtomicUsize>,
    replacement_writer_written_bytes: Arc<AtomicUsize>,
    old_writer_persistent_failure_enabled: Arc<AtomicBool>,
    replacement_opened_before_close_ack: AtomicBool,
    behavior: RetirementBehavior,
}

impl RetirementFactory {
    /// 只给第一代 transport 安装故障，后续代保持 reader 打开直到 manager 显式关闭。
    ///
    /// @author kongweiguang
    fn new(behavior: RetirementBehavior) -> Self {
        let old_control = if behavior == RetirementBehavior::RunningCloseWaitsForRelease {
            Arc::new(RetirementControl::with_close_barrier(
                behavior,
                None,
                Arc::new(Notify::new()),
                Arc::new(Notify::new()),
            ))
        } else {
            Arc::new(RetirementControl::new(behavior, None))
        };
        let persistent_writer_failure = matches!(
            behavior,
            RetirementBehavior::TypedDisconnectPersistentBrokenPipe
                | RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce
                | RetirementBehavior::PersistentWriterFailure
        );
        let old_writer_persistent_failure_enabled =
            Arc::new(AtomicBool::new(persistent_writer_failure));
        Self {
            opens: AtomicUsize::new(0),
            old_control,
            old_writer_shutdown_calls: Arc::new(AtomicUsize::new(0)),
            old_writer_write_calls: Arc::new(AtomicUsize::new(0)),
            old_writer_written_bytes: Arc::new(AtomicUsize::new(0)),
            old_remote_channel_end_confirmed: Arc::new(AtomicBool::new(false)),
            replacement_writer_write_calls: Arc::new(AtomicUsize::new(0)),
            replacement_writer_written_bytes: Arc::new(AtomicUsize::new(0)),
            old_writer_persistent_failure_enabled,
            replacement_opened_before_close_ack: AtomicBool::new(false),
            behavior,
        }
    }
}

#[async_trait]
impl SessionTransportFactory for RetirementFactory {
    /// 每次实际 open 都计数，使旧资源未 ACK 时误建替代 transport 可以直接被断言发现。
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
        let open_index = self.opens.fetch_add(1, Ordering::AcqRel);
        if open_index > 0 && !self.old_control.close_acknowledged.load(Ordering::Acquire) {
            self.replacement_opened_before_close_ack
                .store(true, Ordering::Release);
        }
        if open_index == 0 {
            let reader: Pin<Box<dyn AsyncRead + Send>> = if matches!(
                self.behavior,
                RetirementBehavior::Running
                    | RetirementBehavior::RunningCloseFailsOnce
                    | RetirementBehavior::TypedDisconnectPersistentBrokenPipe
                    | RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce
            ) {
                let (reader, peer) = tokio::io::duplex(128);
                *self.old_control.peer.lock().await = Some(peer);
                if matches!(
                    self.behavior,
                    RetirementBehavior::TypedDisconnectPersistentBrokenPipe
                        | RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce
                ) {
                    Box::pin(RemoteCloseReader {
                        inner: reader,
                        remote_channel_end_confirmed: Arc::clone(
                            &self.old_remote_channel_end_confirmed,
                        ),
                    })
                } else {
                    Box::pin(reader)
                }
            } else {
                Box::pin(FailedReader)
            };
            return Ok(SessionTransport {
                reader,
                writer: Box::pin(RetirementWriter {
                    shutdown_calls: self.old_writer_shutdown_calls.clone(),
                    write_calls: self.old_writer_write_calls.clone(),
                    written_bytes: self.old_writer_written_bytes.clone(),
                    fail_first_shutdown: self.behavior
                        == RetirementBehavior::WriterShutdownFailsOnce,
                    persistent_shutdown_error: match self.behavior {
                        RetirementBehavior::TypedDisconnectPersistentBrokenPipe
                        | RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce => {
                            Some(io::ErrorKind::BrokenPipe)
                        }
                        RetirementBehavior::PersistentWriterFailure => Some(io::ErrorKind::Other),
                        _ => None,
                    },
                    persistent_failure_enabled: self.old_writer_persistent_failure_enabled.clone(),
                    remote_channel_end_confirmed: (self.behavior
                        == RetirementBehavior::TypedDisconnectPersistentBrokenPipe
                        || self.behavior
                            == RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce)
                        .then(|| Arc::clone(&self.old_remote_channel_end_confirmed)),
                }),
                control: self.old_control.clone(),
                shell_dialect: RemoteShellDialect::Unknown,
                process_id: None,
                shell_integration_enabled: false,
            });
        }
        let (reader, peer) = tokio::io::duplex(128);
        Ok(SessionTransport::new(
            reader,
            RetirementWriter {
                shutdown_calls: Arc::new(AtomicUsize::new(0)),
                write_calls: self.replacement_writer_write_calls.clone(),
                written_bytes: self.replacement_writer_written_bytes.clone(),
                fail_first_shutdown: false,
                persistent_shutdown_error: None,
                persistent_failure_enabled: Arc::new(AtomicBool::new(false)),
                remote_channel_end_confirmed: None,
            },
            RetirementControl::new(RetirementBehavior::Success, Some(peer)),
        ))
    }
}

/// 通过公开 SessionService 观察实际 Failed 状态，不直接改写 manager 私有生命周期。
async fn create_failed_session(manager: &SessionManager) -> SessionInfo {
    let created = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Ssh(SshSessionTarget {
                host_id: HostId::new(),
                working_directory: None,
                command: None,
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid test terminal size"),
        })
        .await
        .expect("initial transport opens");
    timeout(Duration::from_secs(2), async {
        loop {
            let info = manager
                .list(Default::default())
                .await
                .expect("list initial session")
                .into_iter()
                .find(|info| info.session_id == created.session_id)
                .expect("initial session remains registered");
            if info.lifecycle == SessionLifecycle::Failed {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader failure reaches the public session lifecycle")
}

/// 建立 reader 保持存活的会话，验证重启入口接受 Running 并等待原 transport 关闭。
///
/// @author kongweiguang
async fn create_running_session(manager: &SessionManager) -> SessionInfo {
    manager
        .create(CreateSessionRequest {
            target: SessionTarget::Ssh(SshSessionTarget {
                host_id: HostId::new(),
                working_directory: None,
                command: None,
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid test terminal size"),
        })
        .await
        .expect("initial running transport opens")
}

/// 等待指定 lifecycle 的公开快照，避免测试依赖 reader 任务调度或私有状态写入时点。
///
/// @author kongweiguang
async fn wait_for_session_lifecycle(
    manager: &SessionManager,
    session_id: &kerminal_domain::identity::SessionId,
    lifecycle: SessionLifecycle,
) -> SessionInfo {
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(info) = manager
                .list(Default::default())
                .await
                .expect("list current session")
                .into_iter()
                .find(|info| &info.session_id == session_id && info.lifecycle == lifecycle)
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("session reaches the requested lifecycle")
}

/// 对故障前后都查询真实 registry 和 generation；先完成测试资源清理，再断言失败及重试结果。
async fn assert_retirement_failure_keeps_owner(behavior: RetirementBehavior) {
    let factory = Arc::new(RetirementFactory::new(behavior));
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            close_wait: if behavior == RetirementBehavior::WaitTimesOutOnce {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(1)
            },
            ..SessionRuntimeConfig::default()
        },
    );
    let created = create_failed_session(&manager).await;
    let initial_context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("initial context remains observable");
    let first = timeout(
        Duration::from_secs(2),
        manager.reconnect(created.session_id.clone()),
    )
    .await
    .expect("first retirement attempt is bounded");
    let after_failure = manager
        .list(Default::default())
        .await
        .expect("list after failure");
    let failure_context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("failed retirement retains its session");
    let opens_after_failure = factory.opens.load(Ordering::Acquire);
    let closes_after_failure = factory.old_control.close_calls.load(Ordering::Acquire);
    let waits_after_failure = factory.old_control.wait_calls.load(Ordering::Acquire);

    let retry = timeout(
        Duration::from_secs(2),
        manager.reconnect(created.session_id.clone()),
    )
    .await
    .expect("retirement retry is bounded");
    let retry_context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("retry context remains observable");
    let cleanup = manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await;
    let remaining = manager
        .list(Default::default())
        .await
        .expect("list after cleanup");

    assert!(first.is_err(), "{behavior:?} must stop the first reconnect");
    assert_eq!(after_failure.len(), 1, "old registry owner must remain");
    assert_eq!(after_failure[0].session_id, created.session_id);
    assert_eq!(after_failure[0].owner, created.owner);
    assert_eq!(after_failure[0].target, created.target);
    assert_eq!(after_failure[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(
        failure_context.session_generation,
        initial_context.session_generation
    );
    assert_eq!(
        opens_after_failure, 1,
        "no replacement may open before retirement ACK"
    );
    assert_eq!(
        closes_after_failure,
        if behavior == RetirementBehavior::WriterShutdownFailsOnce {
            0
        } else {
            1
        },
        "writer failure must stop before control close"
    );
    assert_eq!(
        waits_after_failure,
        if matches!(
            behavior,
            RetirementBehavior::WaitFailsOnce | RetirementBehavior::WaitTimesOutOnce
        ) {
            1
        } else {
            0
        },
        "close failure must stop before exit wait"
    );
    let reconnected = retry.expect("the same old transport can retire on retry");
    assert_eq!(reconnected.session_id, created.session_id);
    assert_eq!(reconnected.owner, created.owner);
    assert_eq!(reconnected.target, created.target);
    assert_eq!(reconnected.lifecycle, SessionLifecycle::Running);
    assert_eq!(
        retry_context.session_generation,
        initial_context.session_generation.wrapping_add(1)
    );
    assert_eq!(factory.opens.load(Ordering::Acquire), 2);
    assert_eq!(
        factory.old_control.close_calls.load(Ordering::Acquire),
        if behavior == RetirementBehavior::WriterShutdownFailsOnce {
            1
        } else {
            2
        },
        "retry must retire the original control"
    );
    assert_eq!(
        factory.old_control.wait_calls.load(Ordering::Acquire),
        if matches!(
            behavior,
            RetirementBehavior::WaitFailsOnce | RetirementBehavior::WaitTimesOutOnce
        ) {
            2
        } else {
            1
        }
    );
    assert_eq!(
        factory.old_writer_shutdown_calls.load(Ordering::Acquire),
        if behavior == RetirementBehavior::WriterShutdownFailsOnce {
            2
        } else {
            1
        }
    );
    cleanup.expect("replacement transport closes cleanly");
    assert!(
        remaining.is_empty(),
        "cleanup must release the registry owner"
    );
}

/// close 的失败与退出码未知不同，不能覆盖旧 control 或提前创建新 transport。
#[tokio::test(flavor = "current_thread")]
async fn reconnect_close_failure_keeps_original_transport_for_retry() {
    assert_retirement_failure_keeps_owner(RetirementBehavior::CloseFailsOnce).await;
}

/// 非 typed disconnect 的退出错误仍可能代表 worker 未收尾，必须保留原 owner 重试。
#[tokio::test(flavor = "current_thread")]
async fn reconnect_non_disconnect_wait_failure_keeps_original_transport_for_retry() {
    assert_retirement_failure_keeps_owner(RetirementBehavior::WaitFailsOnce).await;
}

/// 同一绝对退役预算到期时返回失败，后续重试再给同一旧 control 新的明确预算。
#[tokio::test(flavor = "current_thread")]
async fn reconnect_exit_wait_timeout_keeps_original_transport_for_retry() {
    assert_retirement_failure_keeps_owner(RetirementBehavior::WaitTimesOutOnce).await;
}

/// writer 退役失败也必须离开 Starting，确保显式重试无需先删除整条 session。
#[tokio::test(flavor = "current_thread")]
async fn reconnect_writer_shutdown_failure_returns_failed_and_can_retry() {
    assert_retirement_failure_keeps_owner(RetirementBehavior::WriterShutdownFailsOnce).await;
}

/// Exact channel-end proof makes persistent BrokenPipe EOF idempotent; replacement waits for old close ACK and never replays input.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn reconnect_typed_disconnect_skips_persistent_broken_pipe_after_owner_ack_without_replay() {
    let factory = Arc::new(RetirementFactory::new(
        RetirementBehavior::TypedDisconnectBrokenPipeCloseFailsOnce,
    ));
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    );
    let created = create_running_session(&manager).await;
    let input = b"sent-before-disconnect";
    manager
        .write(created.session_id.clone(), input.to_vec())
        .await
        .expect("the initial writer accepts the test input once");
    assert_eq!(
        factory.old_writer_written_bytes.load(Ordering::Acquire),
        input.len()
    );

    let remote_peer = factory.old_control.peer.lock().await.take();
    drop(remote_peer);
    wait_for_session_lifecycle(&manager, &created.session_id, SessionLifecycle::Failed).await;
    let first_attempt = timeout(
        Duration::from_secs(3),
        manager.reconnect(created.session_id.clone()),
    )
    .await
    .expect("reconnect remains bounded");
    let retained = manager
        .list(Default::default())
        .await
        .expect("list after failed old control cleanup ACK");
    assert!(first_attempt.is_err());
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].session_id, created.session_id);
    assert_eq!(retained[0].lifecycle, SessionLifecycle::Failed);
    assert!(
        !factory
            .old_control
            .close_acknowledged
            .load(Ordering::Acquire)
    );
    assert_eq!(factory.opens.load(Ordering::Acquire), 1);
    assert!(
        !factory
            .replacement_opened_before_close_ack
            .load(Ordering::Acquire),
        "a typed writer proof cannot replace the old transport before control ACK"
    );

    let reconnected = timeout(
        Duration::from_secs(3),
        manager.reconnect(created.session_id.clone()),
    )
    .await
    .expect("cleanup retry remains bounded")
    .expect("typed channel end permits replacement only after the retry ACK");

    assert_eq!(reconnected.lifecycle, SessionLifecycle::Running);
    assert_eq!(factory.opens.load(Ordering::Acquire), 2);
    assert!(
        factory
            .old_control
            .close_acknowledged
            .load(Ordering::Acquire)
    );
    assert!(
        !factory
            .replacement_opened_before_close_ack
            .load(Ordering::Acquire),
        "a replacement cannot open before the old control acknowledges close"
    );
    assert_eq!(factory.old_control.close_calls.load(Ordering::Acquire), 2);
    assert!(
        factory
            .old_remote_channel_end_confirmed
            .load(Ordering::Acquire)
    );
    assert_eq!(
        factory.old_writer_shutdown_calls.load(Ordering::Acquire),
        0,
        "the exact reader proof bypasses only the dead underlying writer shutdown"
    );
    assert_eq!(
        factory.old_writer_written_bytes.load(Ordering::Acquire),
        input.len(),
        "the old input was written once before disconnect"
    );
    assert_eq!(
        factory
            .replacement_writer_written_bytes
            .load(Ordering::Acquire),
        0,
        "reconnect does not replay accepted input into the new transport"
    );
    let fresh_input = b"new-input-after-reconnect";
    manager
        .write(created.session_id.clone(), fresh_input.to_vec())
        .await
        .expect("new generation remains writable without inheriting retired channel proof");
    assert_eq!(
        factory
            .replacement_writer_written_bytes
            .load(Ordering::Acquire),
        fresh_input.len(),
        "only new post-reconnect input reaches the replacement writer"
    );

    manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("replacement transport is finally reaped");
    assert!(manager.list(Default::default()).await.unwrap().is_empty());
}

/// CloseAll must use the same exact dead-channel proof and retain the owner until channel/reader cleanup ACKs finish.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn close_typed_disconnect_skips_persistent_broken_pipe_and_reaps_owner() {
    let factory = Arc::new(RetirementFactory::new(
        RetirementBehavior::TypedDisconnectPersistentBrokenPipe,
    ));
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    );
    let created = create_running_session(&manager).await;
    let remote_peer = factory.old_control.peer.lock().await.take();
    drop(remote_peer);
    wait_for_session_lifecycle(&manager, &created.session_id, SessionLifecycle::Failed).await;

    manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("dead SSH writer does not block acknowledged channel cleanup");
    assert!(
        factory
            .old_control
            .close_acknowledged
            .load(Ordering::Acquire)
    );
    assert_eq!(factory.old_control.close_calls.load(Ordering::Acquire), 1);
    assert!(
        factory
            .old_remote_channel_end_confirmed
            .load(Ordering::Acquire)
    );
    assert_eq!(factory.old_writer_shutdown_calls.load(Ordering::Acquire), 0);
    assert_eq!(factory.opens.load(Ordering::Acquire), 1);
    assert!(manager.list(Default::default()).await.unwrap().is_empty());
}

/// 未获 typed disconnect 的普通持久 writer 错误仍拒绝重连并保留同一 owner，修复后可正常清理。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn reconnect_persistent_generic_writer_error_without_disconnect_proof_keeps_owner() {
    let factory = Arc::new(RetirementFactory::new(
        RetirementBehavior::PersistentWriterFailure,
    ));
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    );
    let created = create_failed_session(&manager).await;
    let failure = manager.reconnect(created.session_id.clone()).await;
    let retained = manager
        .list(Default::default())
        .await
        .expect("old owner remains registered after unproved writer failure");

    assert!(failure.is_err());
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].session_id, created.session_id);
    assert_eq!(retained[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(factory.opens.load(Ordering::Acquire), 1);
    assert_eq!(factory.old_control.close_calls.load(Ordering::Acquire), 0);
    assert_eq!(factory.old_writer_shutdown_calls.load(Ordering::Acquire), 1);

    factory
        .old_writer_persistent_failure_enabled
        .store(false, Ordering::Release);
    factory
        .old_control
        .exit_failure_enabled
        .store(false, Ordering::Release);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("retained owner can be reaped after the ordinary writer fault clears");
    assert!(manager.list(Default::default()).await.unwrap().is_empty());
}

/// 已 ACK 的关闭允许 typed disconnect 缺少退出码，保留既有 SSH 原位恢复语义。
#[tokio::test(flavor = "current_thread")]
async fn reconnect_typed_disconnect_requires_close_ack_before_replacement() {
    let factory = Arc::new(RetirementFactory::new(RetirementBehavior::TypedDisconnect));
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    );
    let created = create_failed_session(&manager).await;
    let initial_context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("initial context remains observable");
    let reconnect = manager.reconnect(created.session_id.clone()).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("reconnected context remains observable");
    let cleanup = manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await;

    let reconnected = reconnect.expect("typed disconnect permits a replacement after close ACK");
    assert_eq!(reconnected.session_id, created.session_id);
    assert_eq!(reconnected.owner, created.owner);
    assert_eq!(reconnected.target, created.target);
    assert_eq!(reconnected.lifecycle, SessionLifecycle::Running);
    assert_eq!(
        context.session_generation,
        initial_context.session_generation.wrapping_add(1)
    );
    assert_eq!(factory.old_control.close_calls.load(Ordering::Acquire), 1);
    assert_eq!(factory.old_control.wait_calls.load(Ordering::Acquire), 1);
    assert_eq!(factory.opens.load(Ordering::Acquire), 2);
    cleanup.expect("replacement transport closes cleanly");
}

/// 重启运行中的 SSH session 时保留 session、owner 与 target，且只在旧 control ACK 后推进 generation。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn restart_running_session_preserves_identity_and_advances_generation() {
    let factory = Arc::new(RetirementFactory::new(RetirementBehavior::Running));
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            close_wait: Duration::from_secs(1),
            ..SessionRuntimeConfig::default()
        },
    );
    let created = create_running_session(&manager).await;
    let context_before = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("initial running context is observable");
    let reconnect = manager
        .reconnect_cancellable(created.session_id.clone(), CancellationToken::new())
        .await;
    let context_after_reconnect = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("rejected reconnect leaves running context observable");
    let opens_after_reconnect = factory.opens.load(Ordering::Acquire);
    let restart = timeout(
        Duration::from_secs(2),
        manager.restart_cancellable(created.session_id.clone(), CancellationToken::new()),
    )
    .await
    .expect("running session restart is bounded");
    let context_after = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("restarted context is observable");
    let cleanup = manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await;

    let restarted = restart.expect("running session may restart in place");
    assert!(
        reconnect.is_err(),
        "ordinary reconnect must still reject Running"
    );
    assert_eq!(opens_after_reconnect, 1);
    assert_eq!(
        context_after_reconnect.session_generation,
        context_before.session_generation
    );
    assert_eq!(restarted.session_id, created.session_id);
    assert_eq!(restarted.owner, created.owner);
    assert_eq!(restarted.target, created.target);
    assert_eq!(restarted.lifecycle, SessionLifecycle::Running);
    assert_eq!(
        context_after.session_generation,
        context_before.session_generation.wrapping_add(1)
    );
    assert_eq!(factory.old_control.close_calls.load(Ordering::Acquire), 1);
    // 初代会话发布时 exit observer 已等待一次，restart close ACK 后还会显式等待一次。
    assert_eq!(factory.old_control.wait_calls.load(Ordering::Acquire), 2);
    assert_eq!(factory.opens.load(Ordering::Acquire), 2);
    cleanup.expect("replacement transport closes cleanly");
}

/// 旧 transport 未确认关闭时不创建替代资源；失败保留同一会话 owner 并允许显式重试。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn restart_close_failure_retains_original_owner_until_retry_ack() {
    let factory = Arc::new(RetirementFactory::new(
        RetirementBehavior::RunningCloseFailsOnce,
    ));
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            close_wait: Duration::from_secs(1),
            ..SessionRuntimeConfig::default()
        },
    );
    let created = create_running_session(&manager).await;
    let context_before = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("initial running context is observable");
    let first = manager
        .restart_cancellable(created.session_id.clone(), CancellationToken::new())
        .await;
    let after_failure = manager
        .list(Default::default())
        .await
        .expect("list after failed restart");
    let failure_context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("failed restart retains original context");
    let opens_after_failure = factory.opens.load(Ordering::Acquire);
    let retry = manager
        .restart_cancellable(created.session_id.clone(), CancellationToken::new())
        .await;
    let context_after_retry = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("retried restart context is observable");
    let cleanup = manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await;

    assert!(
        first.is_err(),
        "first close must fail before opening a replacement"
    );
    assert_eq!(after_failure.len(), 1, "original session owner must remain");
    assert_eq!(after_failure[0].session_id, created.session_id);
    assert_eq!(after_failure[0].owner, created.owner);
    assert_eq!(after_failure[0].target, created.target);
    assert_eq!(after_failure[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(
        failure_context.session_generation,
        context_before.session_generation
    );
    assert_eq!(opens_after_failure, 1, "no open before close ACK");
    let restarted = retry.expect("the same original transport can be retired on retry");
    assert_eq!(restarted.session_id, created.session_id);
    assert_eq!(restarted.owner, created.owner);
    assert_eq!(restarted.target, created.target);
    assert_eq!(restarted.lifecycle, SessionLifecycle::Running);
    assert_eq!(
        context_after_retry.session_generation,
        context_before.session_generation.wrapping_add(1)
    );
    assert_eq!(factory.old_control.close_calls.load(Ordering::Acquire), 2);
    // 创建 observer 的等待计数包含在内；失败 close 不会执行 restart 的第二次等待，retry 才会。
    assert_eq!(factory.old_control.wait_calls.load(Ordering::Acquire), 2);
    assert_eq!(factory.opens.load(Ordering::Acquire), 2);
    cleanup.expect("replacement transport closes cleanly");
}

/// 调用方 abort 后，后台 restart 仍完成旧 transport 收尾并把 Starting 收敛为可关闭的 Failed owner。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn aborted_restart_finishes_retirement_without_losing_cleanup_owner() {
    let factory = Arc::new(RetirementFactory::new(
        RetirementBehavior::RunningCloseWaitsForRelease,
    ));
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            close_wait: Duration::from_secs(1),
            ..SessionRuntimeConfig::default()
        },
    ));
    let created = create_running_session(&manager).await;
    let context_before = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("initial context is observable");
    let restart_manager = Arc::clone(&manager);
    let restart_id = created.session_id.clone();
    let caller = tokio::spawn(async move {
        restart_manager
            .restart_cancellable(restart_id, CancellationToken::new())
            .await
    });

    timeout(
        Duration::from_secs(2),
        factory
            .old_control
            .close_entered
            .as_ref()
            .expect("test factory installed close barrier")
            .notified(),
    )
    .await
    .expect("restart reached old transport close barrier");
    let during_restart = manager
        .list(Default::default())
        .await
        .expect("session remains registered while closing");
    assert_eq!(during_restart[0].lifecycle, SessionLifecycle::Starting);

    caller.abort();
    assert!(
        caller
            .await
            .expect_err("caller task was aborted")
            .is_cancelled()
    );
    factory
        .old_control
        .close_release
        .as_ref()
        .expect("test factory installed close barrier")
        .notify_one();

    let settled = timeout(Duration::from_secs(2), async {
        loop {
            let sessions = manager
                .list(Default::default())
                .await
                .expect("list after caller abort");
            if sessions[0].lifecycle == SessionLifecycle::Failed {
                break sessions;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("manager-owned restart converges after cancellation");
    let context_after = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("old session owner remains observable after cancellation");
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].session_id, created.session_id);
    assert_eq!(settled[0].owner, created.owner);
    assert_eq!(settled[0].target, created.target);
    assert_eq!(settled[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(
        context_after.session_generation,
        context_before.session_generation
    );
    assert_eq!(factory.opens.load(Ordering::Acquire), 1);

    manager
        .close_all_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("retained original transport owner closes cleanly");
    assert!(
        manager
            .list(Default::default())
            .await
            .expect("list after close")
            .is_empty()
    );
}
