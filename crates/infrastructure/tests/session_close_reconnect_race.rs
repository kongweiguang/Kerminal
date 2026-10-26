//! 覆盖 session close 收尾期间对同一 SessionId 重连的排斥规则。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::sessions::{
    CreateSessionRequest, RenderSnapshotRequest, ScrollRequest, SessionService, SessionTransport,
    SessionTransportControl, SessionTransportFactory, SessionTransportOpenError,
    TerminalSearchRequest, UiSessionBinding, UiTerminalSearchRequest,
};
use kerminal_application::{
    AppError, AppResult,
    sessions::logging::{OutputRecordResult, SessionOutputRecorder},
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    sessions::{
        LocalSessionTarget, SessionExitStatus, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex as AsyncMutex, Notify, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

struct RaceFactory {
    opens: AtomicUsize,
    controls: AsyncMutex<Vec<Arc<RaceControl>>>,
    cancel_wait_exit_after_close: bool,
}

impl RaceFactory {
    /// 独立计数每次 open，令竞态测试能断言被关闭的 SessionId 没有被重连复用。
    ///
    /// @author kongweiguang
    fn new() -> Self {
        Self {
            opens: AtomicUsize::new(0),
            controls: AsyncMutex::new(Vec::new()),
            cancel_wait_exit_after_close: false,
        }
    }

    /// 模拟 SSH PTY 在 close ACK 后以 Cancelled 结束 reader，复现退出码缺失但资源已回收的合法结果。
    ///
    /// @author kongweiguang
    fn cancelled_wait_exit_after_close() -> Self {
        Self {
            opens: AtomicUsize::new(0),
            controls: AsyncMutex::new(Vec::new()),
            cancel_wait_exit_after_close: true,
        }
    }
}

#[async_trait]
impl SessionTransportFactory for RaceFactory {
    /// 交付仍保持打开的测试流；可选择模拟 SSH 主动关闭后的 Cancelled 退出结果。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        self.opens.fetch_add(1, Ordering::AcqRel);
        let (reader, peer) = tokio::io::duplex(128);
        let control = Arc::new(RaceControl::new(peer, self.cancel_wait_exit_after_close));
        self.controls.lock().await.push(Arc::clone(&control));
        Ok(SessionTransport {
            reader: Box::pin(reader),
            writer: Box::pin(tokio::io::sink()),
            control,
            shell_dialect: kerminal_domain::connections::RemoteShellDialect::Unknown,
            process_id: None,
            shell_integration_enabled: false,
        })
    }
}

struct RaceControl {
    peer: AsyncMutex<Option<tokio::io::DuplexStream>>,
    cancel_wait_exit_after_close: bool,
}

impl RaceControl {
    /// 将对端流保留至 close，并可模拟 SSH 主动关闭时缺少远端退出码的结果。
    ///
    /// @author kongweiguang
    fn new(peer: tokio::io::DuplexStream, cancel_wait_exit_after_close: bool) -> Self {
        Self {
            peer: AsyncMutex::new(Some(peer)),
            cancel_wait_exit_after_close,
        }
    }
}

#[async_trait]
impl SessionTransportControl for RaceControl {
    /// 测试终端不改变伪 PTY 的实际容量，但满足 resize 生命周期契约。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 关闭 duplex 对端以释放挂起 reader；重复调用保持幂等。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.peer.lock().await.take();
        Ok(())
    }

    /// 按 fixture 模式返回成功状态或主动 close 后的 Cancelled，覆盖 SSH 的退出码缺失语义。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        if self.cancel_wait_exit_after_close && self.peer.lock().await.is_none() {
            return Err(AppError::cancelled("SSH PTY 会话已关闭或取消"));
        }
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

/// close_until 已确认释放时，SSH reader 的 Cancelled 退出不应让 session 留在 Closing registry。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn confirmed_close_treats_ssh_cancelled_exit_as_unknown_status() {
    let factory = Arc::new(RaceFactory::cancelled_wait_exit_after_close());
    let manager = SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    );
    let created = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("ssh-close-cancel-regression"),
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
        .expect("fake SSH session opens");
    let control = factory
        .controls
        .lock()
        .await
        .first()
        .cloned()
        .expect("test transport control exists");

    manager
        .close_until(
            created.session_id.clone(),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("successful close ACK proves cleanup despite missing exit status");
    assert!(
        control.peer.lock().await.is_none(),
        "close ACK released the peer"
    );
    assert!(
        manager
            .list(Default::default())
            .await
            .expect("read remaining sessions")
            .is_empty()
    );
    let repeated_close = manager
        .close_until(created.session_id, Instant::now() + Duration::from_secs(5))
        .await
        .expect_err("closed session must not remain registered");
    assert_eq!(
        repeated_close.public_code(),
        kerminal_application::PublicErrorCode::NotFound
    );
}

struct BlockingRecorder {
    finish_started: Notify,
    release: AsyncMutex<Option<oneshot::Receiver<()>>>,
    finish_calls: AtomicUsize,
}

impl BlockingRecorder {
    /// 只阻塞一次 finish，精确停在 session 已 Closed、registry 尚待清理的窗口。
    ///
    /// @author kongweiguang
    fn new(release: oneshot::Receiver<()>) -> Self {
        Self {
            finish_started: Notify::new(),
            release: AsyncMutex::new(Some(release)),
            finish_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionOutputRecorder for BlockingRecorder {
    /// 本测试不录制字节，仅用 finish_session 控制 close 的可观测时序。
    ///
    /// @author kongweiguang
    fn try_record_output(
        &self,
        _session_id: &kerminal_domain::identity::SessionId,
        _sequence: u64,
        _bytes: &[u8],
    ) -> OutputRecordResult {
        OutputRecordResult::Ignored
    }

    /// 暂停 recorder 收尾，使并发重连观察到已结束但尚未从 registry 移除的 session。
    ///
    /// @author kongweiguang
    async fn finish_session(
        &self,
        _session_id: &kerminal_domain::identity::SessionId,
        _deadline: std::time::Instant,
    ) -> AppResult<kerminal_application::sessions::logging::SessionLogState> {
        if self.finish_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            if let Some(release) = self.release.lock().await.take() {
                self.finish_started.notify_one();
                let _ = release.await;
            }
            return Err(kerminal_application::AppError::infrastructure(
                "test recorder flush fails once to exercise close retry",
            ));
        }
        Ok(kerminal_application::sessions::logging::SessionLogState::inactive())
    }

    /// 测试 recorder 没有后台 writer，shutdown 不需额外资源收尾。
    ///
    /// @author kongweiguang
    async fn shutdown_until(&self, _deadline: std::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

/// recorder.finish 明确失败后只保留原结束视图的只读能力，并在断言前完成 close 清理。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn closed_session_cannot_reconnect_while_recorder_finishes() {
    let (release_sender, release_receiver) = oneshot::channel();
    let recorder = Arc::new(BlockingRecorder::new(release_receiver));
    let factory = Arc::new(RaceFactory::new());
    let manager = Arc::new(
        SessionManager::new(
            factory.clone(),
            tokio::runtime::Handle::current(),
            SessionRuntimeConfig::default(),
        )
        .with_output_recorder(recorder.clone()),
    );
    let created = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("session-race-test"),
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
        .expect("fake terminal session opens");
    let session_id = created.session_id.clone();
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("capture the visible UI generation before exit");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: created.owner.clone(),
        session_generation: context.session_generation,
    };
    let first_control = factory
        .controls
        .lock()
        .await
        .first()
        .cloned()
        .expect("first fake transport control exists");
    let mut first_peer = first_control
        .peer
        .lock()
        .await
        .take()
        .expect("first fake transport peer exists");
    first_peer
        .write_all(b"retained history marker\r\n")
        .await
        .expect("fake transport accepts deterministic history output");
    drop(first_peer);
    let exited = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let current = manager
                .list(Default::default())
                .await
                .expect("read lifecycle while fake reader observes EOF")
                .into_iter()
                .find(|info| info.session_id == session_id)
                .expect("session remains registered until explicit close");
            if current.lifecycle == SessionLifecycle::Exited {
                break current;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fake EOF retires the original UI transport");
    assert_eq!(exited.lifecycle, SessionLifecycle::Exited);

    let close_manager = Arc::clone(&manager);
    let close_session_id = session_id.clone();
    let close_task = tokio::spawn(async move {
        close_manager
            .close_until(close_session_id, Instant::now() + Duration::from_secs(10))
            .await
    });
    let finish_started = recorder.finish_started.notified();
    tokio::time::timeout(Duration::from_secs(3), finish_started)
        .await
        .expect("close reaches recorder finalization after transport has closed");

    assert!(
        manager
            .scroll_ui(binding.clone(), ScrollRequest { lines: 1 })
            .await
            .is_err(),
        "history must not reopen while recorder completion is still pending"
    );

    let closed_snapshot = manager.list(Default::default()).await;
    let reconnect = tokio::time::timeout(
        Duration::from_secs(3),
        manager.reconnect(session_id.clone()),
    )
    .await;

    // 总在断言前释放 barrier；即使回归重现，也让 close task 和测试 transport 完成清理。
    let _ = release_sender.send(());
    let close_result = tokio::time::timeout(Duration::from_secs(5), close_task)
        .await
        .expect("close finishes after recorder release")
        .expect("close task does not panic");
    let still_registered = manager.list(Default::default()).await;
    let retry_reconnect = manager.reconnect(session_id.clone()).await;

    // 先把所有正向历史读取和负向写入校验收集成 Result；无论观察成功与否，都必须先释放会话资源。
    let history_observations = async {
        manager
            .scroll_ui(binding.clone(), ScrollRequest { lines: 1 })
            .await?;
        manager
            .search_ui_cancellable(
                binding.clone(),
                UiTerminalSearchRequest {
                    request: TerminalSearchRequest {
                        query: "retained history marker".to_owned(),
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
            .await?;
        manager.select_all_ui(binding.clone()).await?;
        let copied_text = manager.selected_text_ui(binding.clone()).await?;
        manager
            .resize_ui_view(
                binding.clone(),
                TerminalSize::new(79, 24, 0, 0).expect("valid history resize"),
            )
            .await?;
        manager
            .render_snapshot_ui(binding.clone(), RenderSnapshotRequest::default())
            .await?;
        let writes_rejected = manager.write(session_id.clone(), Vec::new()).await.is_err();
        let pty_resize_rejected = manager
            .resize(
                session_id.clone(),
                TerminalSize::new(79, 25, 0, 0).expect("valid transport resize"),
            )
            .await
            .is_err();
        let mut stale_binding = binding.clone();
        stale_binding.session_generation = stale_binding.session_generation.wrapping_add(1);
        let stale_generation_rejected = manager
            .scroll_ui(stale_binding, ScrollRequest { lines: 1 })
            .await
            .is_err();
        Ok::<_, kerminal_application::AppError>((
            copied_text,
            writes_rejected,
            pty_resize_rejected,
            stale_generation_rejected,
        ))
    }
    .await;
    let retry_close = manager
        .close_until(session_id.clone(), Instant::now() + Duration::from_secs(5))
        .await;
    let controls = factory.controls.lock().await.clone();
    for control in controls {
        let _ = control
            .close_until(Instant::now() + Duration::from_secs(2))
            .await;
    }
    let shutdown = manager
        .close_all_until(Instant::now() + Duration::from_secs(5))
        .await;

    let closed_snapshot =
        closed_snapshot.expect("closed session remains queryable while log flushes");
    assert_eq!(closed_snapshot.len(), 1);
    assert_eq!(closed_snapshot[0].lifecycle, SessionLifecycle::Closed);
    assert!(
        reconnect.is_ok_and(|result| result.is_err()),
        "a Closed session must reject reconnect until its close cleanup finishes"
    );
    assert!(
        close_result.is_err(),
        "first recorder flush failure is returned"
    );
    let (copied_text, writes_rejected, pty_resize_rejected, stale_generation_rejected) =
        history_observations.expect("retained closed history observations succeed");
    assert!(
        copied_text.is_some_and(|text| text.contains("retained history marker")),
        "copy returns the actual selected output from the retained history"
    );
    assert!(
        writes_rejected,
        "closed history access must never reopen transport writes"
    );
    assert!(
        pty_resize_rejected,
        "closed history access must never re-enable PTY resize"
    );
    assert!(
        stale_generation_rejected,
        "wrong generation cannot read the closed history view"
    );
    let still_registered =
        still_registered.expect("failed recorder flush keeps its session for retry");
    assert_eq!(still_registered.len(), 1);
    assert_eq!(still_registered[0].lifecycle, SessionLifecycle::Closed);
    assert!(
        retry_reconnect.is_err(),
        "Closed remains unavailable after flush failure"
    );
    retry_close.expect("a later close retries recorder finalization");
    assert_eq!(factory.opens.load(Ordering::Acquire), 1);
    shutdown.expect("manager shutdown completes after explicit cleanup");
    assert!(
        manager
            .list(Default::default())
            .await
            .expect("list")
            .is_empty()
    );
}
