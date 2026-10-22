//! 验证 SessionManager 消费运行时 autoReconnect 设置并重用原 SessionId。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::sessions::{
    CreateSessionRequest, SessionService, SessionTransport, SessionTransportControl,
    SessionTransportFactory,
};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::identity::{HostId, PaneId, TabId};
use kerminal_domain::sessions::{
    SessionExitStatus, SessionOwner, SessionTarget, SshSessionTarget, TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct ReconnectingFactory {
    open_count: AtomicUsize,
    failures_before_stable: usize,
}

impl ReconnectingFactory {
    /// 只跟踪 provider 实际 open 次数，使测试能验证自动重连复用原 registry entry。
    ///
    /// @author kongweiguang
    fn new(failures_before_stable: usize) -> Self {
        Self {
            open_count: AtomicUsize::new(0),
            failures_before_stable,
        }
    }
}

#[async_trait]
impl SessionTransportFactory for ReconnectingFactory {
    /// 首次交付立即 EOF 且报告 typed disconnect；重连后保留 reader 直到 manager 关闭。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<SessionTransport, kerminal_application::sessions::SessionTransportOpenError> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test open cancelled").into());
        }
        let open_number = self.open_count.fetch_add(1, Ordering::AcqRel) + 1;
        let (reader, peer) = tokio::io::duplex(128);
        let retryable_disconnect = open_number <= self.failures_before_stable;
        let keep_peer = if retryable_disconnect {
            drop(peer);
            None
        } else {
            Some(peer)
        };
        Ok(SessionTransport::new(
            reader,
            tokio::io::sink(),
            ReconnectingControl {
                peer: Mutex::new(keep_peer),
                retryable_disconnect,
            },
        ))
    }
}

struct ReconnectingControl {
    peer: Mutex<Option<tokio::io::DuplexStream>>,
    retryable_disconnect: bool,
}

#[async_trait]
impl SessionTransportControl for ReconnectingControl {
    /// 测试 transport 无独立 PTY 几何，resize 仅确认 manager 调用了 provider。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 关闭 reader 对端以解除测试 duplex，真实资源仍由唯一 SessionManager 收拢。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.peer.lock().await.take();
        Ok(())
    }

    /// 首次连接模拟非主动远端断线，后续连接返回实际测试退出状态。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        if self.retryable_disconnect {
            return Err(AppError::retryable_remote_disconnect(
                std::io::Error::other("test remote channel closed"),
            ));
        }
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

/// 以明确 UI owner 创建交互式 SSH session，设置与测试目标保持真实领域类型。
///
/// @author kongweiguang
fn create_request() -> CreateSessionRequest {
    CreateSessionRequest {
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
    }
}

/// 等待 provider 达到指定 open 次数，失败时带有限期限而不靠固定慢速 sleep 猜测。
///
/// @author kongweiguang
async fn wait_for_open_count(factory: &ReconnectingFactory, expected: usize) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while factory.open_count.load(Ordering::Acquire) < expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("provider reaches expected open count");
}

/// settings 开启后只重试 typed disconnect，并保持相同 SessionId 和 UI owner。
///
/// @author kongweiguang
#[tokio::test]
async fn enabled_setting_reconnects_same_ui_session_after_typed_disconnect() {
    let factory = Arc::new(ReconnectingFactory::new(1));
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    manager
        .set_auto_reconnect_enabled(true)
        .await
        .expect("manager accepts the settings snapshot");
    let created = manager
        .create(create_request())
        .await
        .expect("interactive SSH test session opens");

    wait_for_open_count(&factory, 2).await;
    let sessions = manager
        .list(Default::default())
        .await
        .expect("session list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, created.session_id);
    assert_eq!(sessions[0].owner, created.owner);
    assert_eq!(factory.open_count.load(Ordering::Acquire), 2);

    manager
        .close_until(created.session_id, Instant::now() + Duration::from_secs(5))
        .await
        .expect("reconnected session closes cleanly");
}

/// settings 切换为关闭后取消退避任务，不能在等待期结束时重新开 SSH channel。
///
/// @author kongweiguang
#[tokio::test]
async fn disabling_setting_cancels_a_queued_retry() {
    let factory = Arc::new(ReconnectingFactory::new(1));
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    manager
        .set_auto_reconnect_enabled(true)
        .await
        .expect("manager accepts the settings snapshot");
    manager
        .create(create_request())
        .await
        .expect("interactive SSH test session opens");
    tokio::time::sleep(Duration::from_millis(100)).await;

    manager
        .set_auto_reconnect_enabled(false)
        .await
        .expect("manager cancels the waiting retry");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(factory.open_count.load(Ordering::Acquire), 1);

    manager
        .close_all_until(Instant::now() + Duration::from_secs(5))
        .await
        .expect("failed session is still explicitly closeable");
}

/// 快速重连确认不会重置同一不稳定周期的尝试序号，从而继续按更长退避间隔运行。
///
/// @author kongweiguang
#[tokio::test]
async fn rapid_disconnects_keep_the_same_backoff_incident() {
    let factory = Arc::new(ReconnectingFactory::new(3));
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    manager
        .set_auto_reconnect_enabled(true)
        .await
        .expect("manager accepts the settings snapshot");
    manager
        .create(create_request())
        .await
        .expect("interactive SSH test session opens");

    let started = Instant::now();
    wait_for_open_count(&factory, 3).await;
    assert!(started.elapsed() >= Duration::from_millis(2800));
    manager
        .close_all_until(Instant::now() + Duration::from_secs(5))
        .await
        .expect("rapidly reconnecting session closes cleanly");
}
