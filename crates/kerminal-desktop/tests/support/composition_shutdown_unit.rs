//! 覆盖 composition 在 PTY 解除阻塞前不串行等待 tracked application tasks。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult,
    executor::ApplicationTaskExecutor,
    sessions::{
        CreateSessionRequest, SessionTransport, SessionTransportControl, SessionTransportFactory,
        SessionTransportOpenError,
    },
};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionOwner, SessionTarget, TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::{
    runtime::Handle,
    sync::{Notify, oneshot},
    time::Instant,
};

use super::{join_session_dependent_shutdowns, shutdown_mcp_before_agent_shutdown};

struct TestSessionControl {
    session_close_signal: Arc<Notify>,
    _peer: Mutex<Option<tokio::io::DuplexStream>>,
}

impl SessionTransportControl for TestSessionControl {
    /// 测试 transport 没有窗口管理器，保留接口但不引入额外异步工作。
    fn resize<'life0, 'async_trait>(
        &'life0 self,
        _size: TerminalSize,
    ) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(()) })
    }

    /// 模拟关闭 PTY 后解除等待该 I/O 的 tracked application task。
    fn close_until<'life0, 'async_trait>(
        &'life0 self,
        _deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self._peer.lock().expect("test peer mutex").take();
            self.session_close_signal.notify_one();
            Ok(())
        })
    }

    /// 测试进程在控制通道关闭后立即报告退出，避免把回归用例变成真实 shell 测试。
    fn wait_exit<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = AppResult<SessionExitStatus>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async {
            Ok(SessionExitStatus {
                code: Some(0),
                signal: None,
                success: true,
            })
        })
    }
}

struct TestSessionFactory {
    control: Arc<TestSessionControl>,
}

impl SessionTransportFactory for TestSessionFactory {
    /// 提供真实 SessionManager 所拥有的异步双向 transport，不启动外部进程。
    fn open<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _target: &'life1 SessionTarget,
        _size: TerminalSize,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<SessionTransport, SessionTransportOpenError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let (stream, peer) = tokio::io::duplex(1024);
            *self.control._peer.lock().expect("test peer mutex") = Some(peer);
            let (reader, writer) = tokio::io::split(stream);
            Ok(SessionTransport {
                reader: Box::pin(reader),
                writer: Box::pin(writer),
                control: Arc::clone(&self.control) as Arc<dyn SessionTransportControl>,
                shell_dialect: kerminal_domain::connections::RemoteShellDialect::Unknown,
                process_id: None,
                shell_integration_enabled: false,
            })
        })
    }
}

/// 让真实 executor tracker 中的任务等待 session-close 信号，固定两类资源必须并行关闭。
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn session_close_unblocks_tracked_task_during_shared_shutdown_join() {
    let executor = ApplicationTaskExecutor::new(Handle::current());
    let close_signal = Arc::new(Notify::new());
    let control = Arc::new(TestSessionControl {
        session_close_signal: Arc::clone(&close_signal),
        _peer: Mutex::new(None),
    });
    let sessions = SessionManager::new(
        Arc::new(TestSessionFactory {
            control: Arc::clone(&control),
        }),
        Handle::current(),
        SessionRuntimeConfig::default(),
    );
    sessions
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(80, 24, 0, 0).expect("valid test grid"),
            initial_palette: None,
        })
        .await
        .expect("fake session should be registered");

    let (started_sender, started_receiver) = oneshot::channel();

    let task_executor = executor.clone();
    let tracked_task = tokio::spawn(async move {
        task_executor
            .run(move |_cancel| async move {
                let _ = started_sender.send(());
                close_signal.notified().await;
                Ok(())
            })
            .await
    });
    started_receiver
        .await
        .expect("tracked operation should start before shutdown");

    let deadline = Instant::now() + Duration::from_secs(2);
    let (executor_result, session_result, rdp_result) = join_session_dependent_shutdowns(
        executor.shutdown_until(deadline),
        sessions.close_all_until(deadline),
        async { Ok::<(), kerminal_application::AppError>(()) },
    )
    .await;

    assert!(
        executor_result.is_ok(),
        "session close must unblock task join"
    );
    assert!(session_result.is_ok());
    assert!(rdp_result.is_ok());
    assert!(tracked_task.await.expect("task waiter should join").is_ok());
}

/// MCP endpoint 同步未完成时不关闭 Agent 写入口，保留同一 controller 的重试路径。
#[tokio::test]
async fn mcp_shutdown_failure_keeps_agent_endpoint_cleanup_retryable() {
    let agent_shutdown_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent_shutdown_flag = Arc::clone(&agent_shutdown_started);

    let result = shutdown_mcp_before_agent_shutdown(
        async { Err(AppError::timeout("MCP endpoint sync")) },
        move || agent_shutdown_flag.store(true, std::sync::atomic::Ordering::Release),
    )
    .await;

    assert!(result.is_err());
    assert!(!agent_shutdown_started.load(std::sync::atomic::Ordering::Acquire));
}

/// 等待真实 MCP shutdown 完成后才关闭 Agent，确保 workspace endpoint clear 排在写门禁关闭前。
#[tokio::test]
async fn agent_shutdown_begins_only_after_mcp_shutdown_completes() {
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let agent_shutdown_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent_shutdown_flag = Arc::clone(&agent_shutdown_started);

    let shutdown = tokio::spawn(async move {
        shutdown_mcp_before_agent_shutdown(
            async move {
                let _ = started_sender.send(());
                release_receiver
                    .await
                    .expect("MCP test shutdown should be released");
                Ok(())
            },
            move || agent_shutdown_flag.store(true, std::sync::atomic::Ordering::Release),
        )
        .await
    });

    started_receiver
        .await
        .expect("MCP shutdown should start first");
    assert!(!agent_shutdown_started.load(std::sync::atomic::Ordering::Acquire));
    release_sender.send(()).expect("release MCP shutdown");
    assert!(shutdown.await.expect("shutdown phase should join").is_ok());
    assert!(agent_shutdown_started.load(std::sync::atomic::Ordering::Acquire));
}
