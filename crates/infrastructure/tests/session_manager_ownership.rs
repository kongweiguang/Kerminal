//! 覆盖取消后的 transport 所有权与容量释放契约。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::AppResult;
use kerminal_application::sessions::{
    CreateSessionRequest, SessionOpenCleanupOwner, SessionService, SessionTransport,
    SessionTransportControl, SessionTransportFactory, SessionTransportOpenError,
};
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionOwner, SessionTarget, TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct TestControl {
    close_calls: Arc<AtomicUsize>,
    first_close: Notify,
}

#[async_trait]
impl SessionTransportControl for TestControl {
    /// 记录 manager 释放迟到 transport 的次数；测试控制没有独立 child 资源。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 让回归测试能证明取消后 manager 仍调用 transport 清理入口。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        if self.close_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            self.first_close.notify_one();
            return Err(kerminal_application::AppError::infrastructure(
                "test cleanup fails once to exercise manager retry",
            ));
        }
        Ok(())
    }

    /// 测试 transport 在 close 后立即报告 child/channel 退出。
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

struct DelayedFactory {
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    control: Arc<TestControl>,
    open_calls: Option<Arc<AtomicUsize>>,
}

#[async_trait]
impl SessionTransportFactory for DelayedFactory {
    /// 故意忽略取消直到测试释放 gate，验证 manager 不会 drop 正在创建 transport 的 future。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        if let Some(open_calls) = &self.open_calls {
            open_calls.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(started) = self.started.lock().expect("factory mutex").take() {
            let _ = started.send(());
        }
        let release = self.release.lock().await.take();
        if let Some(release) = release {
            let _ = release.await;
        }
        Ok(SessionTransport {
            reader: Box::pin(tokio::io::empty()),
            writer: Box::pin(tokio::io::sink()),
            control: self.control.clone(),
            shell_dialect: kerminal_domain::connections::RemoteShellDialect::Unknown,
            process_id: None,
            shell_integration_enabled: false,
        })
    }
}

struct RejectFactory;

#[async_trait]
impl SessionTransportFactory for RejectFactory {
    /// 该 factory 只用于证明 per-call factory 覆盖 manager 默认路由。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        Err(kerminal_application::AppError::unsupported("默认测试 factory 不应被调用").into())
    }
}

struct FailOnceOpenCleanupOwner {
    close_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SessionOpenCleanupOwner for FailOnceOpenCleanupOwner {
    /// 保留未交付 open 资源的清理失败一次，再允许下一轮释放，以固定容量 permit 重试契约。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        if self.close_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            Err(kerminal_application::AppError::infrastructure(
                "test open cleanup fails once",
            ))
        } else {
            Ok(())
        }
    }
}

struct SuccessfulOpenCleanupOwner {
    close_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SessionOpenCleanupOwner for SuccessfulOpenCleanupOwner {
    /// 记录未交付资源的一次成功回收，让回归用例只验证清理完成后的 ID 重试语义。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.close_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct FailOnceThenOpenFactory {
    cleanup: Arc<SuccessfulOpenCleanupOwner>,
    control: Arc<TestControl>,
    open_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SessionTransportFactory for FailOnceThenOpenFactory {
    /// 首次失败并成功回收未交付资源，第二次打开成功，以证明未发布 ID 可在清理完成后重试。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        if self.open_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            let cleanup: Arc<dyn SessionOpenCleanupOwner> = self.cleanup.clone();
            return Err(SessionTransportOpenError::with_cleanup(
                kerminal_application::AppError::infrastructure("test first open failed"),
                cleanup,
            ));
        }

        Ok(SessionTransport {
            reader: Box::pin(tokio::io::empty()),
            writer: Box::pin(tokio::io::sink()),
            control: self.control.clone(),
            shell_dialect: kerminal_domain::connections::RemoteShellDialect::Unknown,
            process_id: None,
            shell_integration_enabled: false,
        })
    }
}

struct FailedOpenFactory {
    cleanup: Arc<FailOnceOpenCleanupOwner>,
}

#[async_trait]
impl SessionTransportFactory for FailedOpenFactory {
    /// 返回显式 cleanup owner，验证 Manager 在失败时保留资源与容量 slot。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let cleanup: Arc<dyn SessionOpenCleanupOwner> = self.cleanup.clone();
        Err(SessionTransportOpenError::with_cleanup(
            kerminal_application::AppError::infrastructure("test factory open failed"),
            cleanup,
        ))
    }
}

/// published-ID tombstone 覆盖普通自动 ID 与显式 ID，并在关闭后仍阻止旧 binding ABA。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn published_id_conflicts_while_live_and_after_close_before_provider_open() {
    let control = Arc::new(TestControl {
        close_calls: Arc::new(AtomicUsize::new(1)),
        first_close: Notify::new(),
    });
    let open_calls = Arc::new(AtomicUsize::new(0));
    let factory: Arc<dyn SessionTransportFactory> = Arc::new(DelayedFactory {
        started: Mutex::new(None),
        release: tokio::sync::Mutex::new(None),
        control,
        open_calls: Some(Arc::clone(&open_calls)),
    });
    let manager = SessionManager::new(
        factory,
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    );
    let request = CreateSessionRequest {
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
        initial_palette: None,
    };
    let first = manager
        .create(request.clone())
        .await
        .expect("first session publishes");
    assert_eq!(open_calls.load(Ordering::Acquire), 1);

    let published_id = first.session_id.clone();
    let error = manager
        .create_with_session_id(
            request.clone(),
            published_id.clone(),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        error
            .expect_err("live session ID must conflict")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(
        open_calls.load(Ordering::Acquire),
        1,
        "provider is not opened for a live ID"
    );
    manager
        .close_until(
            published_id.clone(),
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .expect("close the published session");

    let error = manager
        .create_with_session_id(request, published_id, CancellationToken::new())
        .await;
    assert_eq!(
        error
            .expect_err("published ID remains unavailable after close")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(
        open_calls.load(Ordering::Acquire),
        1,
        "provider is not opened for a retired ID"
    );
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("finish manager shutdown");
}

/// 未发布创建失败且 provider owner 已成功回收时，原 ID reservation 随失败任务释放。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn failed_open_with_completed_cleanup_can_retry_same_id() {
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let open_calls = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(TestControl {
        close_calls: Arc::new(AtomicUsize::new(1)),
        first_close: Notify::new(),
    });
    let factory: Arc<dyn SessionTransportFactory> = Arc::new(FailOnceThenOpenFactory {
        cleanup: Arc::new(SuccessfulOpenCleanupOwner {
            close_calls: Arc::clone(&cleanup_calls),
        }),
        control,
        open_calls: Arc::clone(&open_calls),
    });
    let manager = SessionManager::new(
        factory,
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    );
    let request = CreateSessionRequest {
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
        initial_palette: None,
    };
    let session_id = SessionId::new();

    let first = manager
        .create_with_session_id(
            request.clone(),
            session_id.clone(),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        first
            .expect_err("first provider open fails after owner cleanup succeeds")
            .public_code(),
        kerminal_application::PublicErrorCode::Infrastructure
    );
    assert_eq!(cleanup_calls.load(Ordering::Acquire), 1);
    assert_eq!(open_calls.load(Ordering::Acquire), 1);

    let retried = manager
        .create_with_session_id(request, session_id.clone(), CancellationToken::new())
        .await
        .expect("same unpublished ID is reusable after cleanup completion");
    assert_eq!(retried.session_id, session_id);
    assert_eq!(open_calls.load(Ordering::Acquire), 2);
    manager
        .close_until(retried.session_id, Instant::now() + Duration::from_secs(3))
        .await
        .expect("close the retried session");
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("finish manager shutdown");
}

/// 并发指定相同 ID 时，只有先登记 reservation 的 manager task 会触达 provider。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn concurrent_preallocated_id_opens_provider_once() {
    let open_calls = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(TestControl {
        close_calls: Arc::new(AtomicUsize::new(1)),
        first_close: Notify::new(),
    });
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let factory: Arc<dyn SessionTransportFactory> = Arc::new(DelayedFactory {
        started: Mutex::new(Some(started_sender)),
        release: tokio::sync::Mutex::new(Some(release_receiver)),
        control,
        open_calls: Some(Arc::clone(&open_calls)),
    });
    let manager = Arc::new(SessionManager::new(
        factory,
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let request = CreateSessionRequest {
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
        initial_palette: None,
    };
    let session_id = SessionId::new();
    let first_manager = Arc::clone(&manager);
    let first_id = session_id.clone();
    let first_request = request.clone();
    let first = tokio::spawn(async move {
        first_manager
            .create_with_session_id(first_request, first_id, CancellationToken::new())
            .await
    });
    started_receiver.await.expect("first provider open started");

    let duplicate = manager
        .create_with_session_id(request, session_id, CancellationToken::new())
        .await;
    assert_eq!(
        duplicate
            .expect_err("creating ID must conflict before provider open")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(open_calls.load(Ordering::Acquire), 1);

    release_sender.send(()).expect("release first provider");
    let created = first
        .await
        .expect("manager task completes")
        .expect("reserved session publishes");
    assert_eq!(open_calls.load(Ordering::Acquire), 1);
    manager
        .close_until(created.session_id, Instant::now() + Duration::from_secs(3))
        .await
        .expect("close the created session");
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("finish manager shutdown");
}

/// 在 create 成功结果已入队但 caller 尚未确认时 drop caller，要求 manager 关闭无人认领的 session。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn queued_create_result_drop_is_closed_by_manager_owner() {
    let close_calls = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(TestControl {
        close_calls: Arc::clone(&close_calls),
        first_close: Notify::new(),
    });
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let factory = Arc::new(DelayedFactory {
        started: Mutex::new(Some(started_sender)),
        release: tokio::sync::Mutex::new(Some(release_receiver)),
        control: Arc::clone(&control),
        open_calls: None,
    });
    let manager = Arc::new(SessionManager::new(
        Arc::new(RejectFactory),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let request = CreateSessionRequest {
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
        initial_palette: None,
    };
    let mut caller =
        Box::pin(manager.create_with_transport_factory(request, factory, CancellationToken::new()));
    let first_poll =
        std::future::poll_fn(|context| std::task::Poll::Ready(caller.as_mut().poll(context))).await;
    assert!(
        first_poll.is_pending(),
        "create must be waiting on its owned factory"
    );

    started_receiver.await.expect("factory open started");
    release_sender.send(()).expect("release delayed factory");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !manager
                .list(Default::default())
                .await
                .expect("list published session")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("manager published session before result acknowledgement");
    // Keep the result receiver unpolled until the manager has queued its response and waits for ack.
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    drop(caller);
    tokio::time::timeout(Duration::from_secs(3), control.first_close.notified())
        .await
        .expect("manager starts cleanup after caller drops");

    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("manager drains abandoned create and cleanup tasks");
    assert!(
        close_calls.load(Ordering::Acquire) >= 1,
        "manager closes a session whose create result was never acknowledged"
    );
    assert!(
        manager
            .list(Default::default())
            .await
            .expect("list sessions")
            .is_empty(),
        "unclaimed session must not remain in the public registry"
    );
}

/// 新 create 先重试待清理 owner；成功回收后释放 max_sessions=1 slot 并启动新 PTY。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn next_create_retries_pending_open_cleanup_before_acquiring_capacity() {
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let cleanup = Arc::new(FailOnceOpenCleanupOwner {
        close_calls: Arc::clone(&cleanup_calls),
    });
    let manager = Arc::new(SessionManager::new(
        Arc::new(RejectFactory),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            maximum_sessions: 1,
            ..SessionRuntimeConfig::default()
        },
    ));
    let request = CreateSessionRequest {
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
        initial_palette: None,
    };

    let first = manager
        .create_with_transport_factory(
            request.clone(),
            Arc::new(FailedOpenFactory {
                cleanup: Arc::clone(&cleanup),
            }),
            CancellationToken::new(),
        )
        .await
        .expect_err("open failure must be reported even while cleanup is retained");
    assert_eq!(
        first.public_code(),
        kerminal_application::PublicErrorCode::Infrastructure
    );
    assert_eq!(cleanup_calls.load(Ordering::Acquire), 1);

    let control = Arc::new(TestControl {
        // 本用例只验证待清理 open owner 的容量恢复；避免复用其它用例的首次 close 故障注入。
        close_calls: Arc::new(AtomicUsize::new(1)),
        first_close: Notify::new(),
    });
    let second = manager
        .create_with_transport_factory(
            request,
            Arc::new(DelayedFactory {
                started: Mutex::new(None),
                release: tokio::sync::Mutex::new(None),
                control,
                open_calls: None,
            }),
            CancellationToken::new(),
        )
        .await
        .expect("pre-create cleanup retry frees the only capacity slot");
    assert_eq!(cleanup_calls.load(Ordering::Acquire), 2);
    assert_eq!(
        manager.list(Default::default()).await.expect("list").len(),
        1
    );

    manager
        .close_until(second.session_id, Instant::now() + Duration::from_secs(3))
        .await
        .expect("close the newly created session");
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("finish manager shutdown");
}

/// factory 忽略取消并在稍后返回带 owner 的错误时，Manager 仍保留并在 shutdown 重试 owner。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn late_failed_open_after_cancellation_keeps_cleanup_owner() {
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let cleanup = Arc::new(FailOnceOpenCleanupOwner {
        close_calls: Arc::clone(&cleanup_calls),
    });
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let factory = Arc::new(LateFailedOpenFactory {
        started: Mutex::new(Some(started_sender)),
        release: tokio::sync::Mutex::new(Some(release_receiver)),
        cleanup,
    });
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&factory) as Arc<dyn SessionTransportFactory>,
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            maximum_sessions: 1,
            ..SessionRuntimeConfig::default()
        },
    ));
    let caller_cancellation = CancellationToken::new();
    let request = CreateSessionRequest {
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
        initial_palette: None,
    };
    let session_id = SessionId::new();
    let caller_manager = Arc::clone(&manager);
    let caller_token = caller_cancellation.clone();
    let caller_session_id = session_id.clone();
    let caller = tokio::spawn(async move {
        caller_manager
            .create_with_session_id(request, caller_session_id, caller_token)
            .await
    });

    started_receiver.await.expect("factory open started");
    caller_cancellation.cancel();
    let cancelled = caller
        .await
        .expect("caller task completes")
        .expect_err("cancelled create is not reported successful");
    assert_eq!(
        cancelled.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );

    let duplicate_while_opening = manager
        .create_with_session_id(
            CreateSessionRequest {
                target: SessionTarget::Local(LocalSessionTarget {
                    program: OsString::from("test-shell"),
                    args: Vec::new(),
                    working_directory: None,
                    environment: BTreeMap::new(),
                }),
                owner: SessionOwner::Mcp,
                size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
                initial_palette: None,
            },
            session_id.clone(),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        duplicate_while_opening
            .expect_err("cancelled provider still owns the reserved ID")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    release_sender.send(()).expect("release factory");
    tokio::time::timeout(Duration::from_secs(3), async {
        while cleanup_calls.load(Ordering::Acquire) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("manager attempted cleanup for the late open result");
    tokio::time::sleep(Duration::from_millis(10)).await;
    let duplicate_while_cleanup_pending = manager
        .create_with_session_id(
            CreateSessionRequest {
                target: SessionTarget::Local(LocalSessionTarget {
                    program: OsString::from("test-shell"),
                    args: Vec::new(),
                    working_directory: None,
                    environment: BTreeMap::new(),
                }),
                owner: SessionOwner::Mcp,
                size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
                initial_palette: None,
            },
            session_id,
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        duplicate_while_cleanup_pending
            .expect_err("pending cleanup owner retains the reserved ID")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(
        cleanup_calls.load(Ordering::Acquire),
        1,
        "collision does not retry or bypass cleanup"
    );
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("shutdown waits for late factory error and retries cleanup");
    assert_eq!(cleanup_calls.load(Ordering::Acquire), 2);
}

struct LateFailedOpenFactory {
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    cleanup: Arc<FailOnceOpenCleanupOwner>,
}

#[async_trait]
impl SessionTransportFactory for LateFailedOpenFactory {
    /// 故意忽略取消直至测试释放 channel，证明 manager-owned open task 不会被提前丢弃。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        if let Some(started) = self.started.lock().expect("factory mutex").take() {
            let _ = started.send(());
        }
        let release = self.release.lock().await.take();
        if let Some(release) = release {
            let _ = release.await;
        }
        let cleanup: Arc<dyn SessionOpenCleanupOwner> = self.cleanup.clone();
        Err(SessionTransportOpenError::with_cleanup(
            kerminal_application::AppError::infrastructure("late fake open failed"),
            cleanup,
        ))
    }
}
