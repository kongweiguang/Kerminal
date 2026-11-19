//! 验证 SSH parser response 写失败时不会先取消读泵：由读泵确认真实退出并保留 typed 断线与自动重连。
//!
//! @author kongweiguang

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::sessions::{
    CreateSessionRequest, SessionService, SessionTransport, SessionTransportControl,
    SessionTransportFactory,
};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::identity::{HostId, PaneId, TabId};
use kerminal_domain::sessions::{
    SessionExitStatus, SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget,
    TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::io::{AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// 首个 transport 的回写立即失败，并保留读端直到测试主动断开，模拟 SSH 发送路径先坏于读路径。
struct Factory {
    opens: AtomicUsize,
    opened: Arc<Notify>,
    exit_observed: Arc<AtomicUsize>,
    exit_notified: Arc<Notify>,
    first_peer: Mutex<Option<DuplexStream>>,
    replacement_peer: Mutex<Option<DuplexStream>>,
    first_write_failed: Arc<Notify>,
}

impl Default for Factory {
    /// 测试只需计数和两个可控对端，不预置任何连接状态。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            opens: AtomicUsize::new(0),
            opened: Arc::new(Notify::new()),
            exit_observed: Arc::new(AtomicUsize::new(0)),
            exit_notified: Arc::new(Notify::new()),
            first_peer: Mutex::new(None),
            replacement_peer: Mutex::new(None),
            first_write_failed: Arc::new(Notify::new()),
        }
    }
}

/// 第一次写回固定失败，避免把测试建立在调度时序或真实网络错误上。
struct FailingWriter {
    failed: Arc<Notify>,
    shutdown_attempted: bool,
}

impl AsyncWrite for FailingWriter {
    /// 通知测试“parser response 已尝试写回”，随后返回断链错误。
    ///
    /// @author kongweiguang
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.failed.notify_one();
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "ssh channel closed",
        )))
    }

    /// 写路径已失败，flush 不再制造额外错误。
    ///
    /// @author kongweiguang
    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// 首次关闭失败用来验证断线原因不会被覆盖；退役重试仍必须取得真实关闭回执。
    ///
    /// @author kongweiguang
    fn poll_shutdown(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if std::mem::replace(&mut self.shutdown_attempted, true) {
            return Poll::Ready(Ok(()));
        }
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "ssh channel closed",
        )))
    }
}

#[async_trait]
impl SessionTransportFactory for Factory {
    /// 首个 transport 使用失败 writer，重连后的 transport 保持可读，以便断言同 SessionId 恢复。
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
        let open_number = self.opens.fetch_add(1, Ordering::AcqRel) + 1;
        self.opened.notify_waiters();
        let (reader, peer) = tokio::io::duplex(4096);
        if open_number == 1 {
            *self.first_peer.lock().await = Some(peer);
            Ok(SessionTransport::new(
                reader,
                FailingWriter {
                    failed: Arc::clone(&self.first_write_failed),
                    shutdown_attempted: false,
                },
                FirstControl {
                    exit_observed: Arc::clone(&self.exit_observed),
                    exit_notified: Arc::clone(&self.exit_notified),
                },
            ))
        } else {
            *self.replacement_peer.lock().await = Some(peer);
            Ok(SessionTransport::new(
                reader,
                tokio::io::sink(),
                StableControl,
            ))
        }
    }
}

struct FirstControl {
    exit_observed: Arc<AtomicUsize>,
    exit_notified: Arc<Notify>,
}

#[async_trait]
impl SessionTransportControl for FirstControl {
    /// 测试 transport 不依赖原生几何。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 读端由测试对端释放，不额外关闭资源。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        Ok(())
    }

    /// 模拟已确认的 SSH 远端断线；只有真实读端结束后的该 typed 错误才允许自动重连。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        self.exit_observed.fetch_add(1, Ordering::AcqRel);
        self.exit_notified.notify_waiters();
        Err(AppError::retryable_remote_disconnect(io::Error::other(
            "test remote channel closed",
        )))
    }
}

struct StableControl;

#[async_trait]
impl SessionTransportControl for StableControl {
    /// 重连后的 transport 尺寸不影响断言。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 关闭重连 transport 时只由保存的对端释放读端。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        Ok(())
    }

    /// 稳定连接返回成功退出，避免再次进入重试队列。
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

/// 以 UI 拥有的交互式 SSH 目标创建会话，使自动重连资格与真实使用路径一致。
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
        initial_palette: None,
    }
}

/// 等待 provider 再次被调用；先订阅通知再检查计数，避免丢掉两次 open 间的短暂通知。
///
/// @author kongweiguang
async fn wait_for_open_count(factory: &Factory, expected: usize) {
    let opened = factory.opened.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        while factory.opens.load(Ordering::Acquire) < expected {
            opened.notified().await;
        }
    })
    .await
    .expect("provider reaches expected open count");
}

/// 读泵在 parser response 写失败后仍须等待真实 EOF，并把 wait_exit 的 typed 断线交给重连门禁。
///
/// 旧实现会先 cancellation.cancel()，读泵读不到真实退出，typed 断线随自动重连一起丢失。
///
/// @author kongweiguang
#[tokio::test]
async fn ssh_parser_write_failure_preserves_typed_disconnect_and_reconnects() {
    let factory = Arc::new(Factory::default());
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    SessionService::set_auto_reconnect_enabled(&*manager, true)
        .await
        .expect("manager accepts the settings snapshot");
    let created = manager
        .create(create_request())
        .await
        .expect("interactive SSH test session opens");

    let mut peer = factory.first_peer.lock().await.take().expect("first peer");
    // 光标位置查询会让 reader 生成必须写回 transport 的 CPR 响应。
    peer.write_all(b"\x1b[6n").await.expect("cpr query");
    tokio::time::timeout(
        Duration::from_secs(2),
        factory.first_write_failed.notified(),
    )
    .await
    .expect("parser response write is attempted");

    // 回写失败本身不触发取消；由读泵观察 EOF 后 wait_exit 给出 typed 断线。
    drop(peer);
    let exit_notified = factory.exit_notified.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let notified = exit_notified.notified();
            tokio::pin!(notified);
            if factory.exit_observed.load(Ordering::Acquire) > 0 {
                break;
            }
            tokio::time::timeout(Duration::from_secs(1), &mut notified)
                .await
                .expect("wait_exit notification");
        }
    })
    .await
    .expect("reader confirms exit through wait_exit");
    wait_for_open_count(&factory, 2).await;
    // provider 被调用不等于新 transport 已发布，继续等待同一 SessionId 回到 Running。
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let sessions = manager
                .list(Default::default())
                .await
                .expect("session list");
            if sessions.iter().any(|session| {
                session.session_id == created.session_id
                    && session.lifecycle == SessionLifecycle::Running
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("replacement transport becomes running");

    let sessions = manager
        .list(Default::default())
        .await
        .expect("session list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, created.session_id);
    assert_eq!(sessions[0].lifecycle, SessionLifecycle::Running);
    drop(factory.replacement_peer.lock().await.take());
    manager.close(created.session_id).await.expect("close");
}

/// 写失败但读端未结束时，确认预算到期只标记失败，不发起新的连接。
///
/// @author kongweiguang
#[tokio::test]
async fn ssh_parser_write_failure_without_eof_does_not_reconnect() {
    let factory = Arc::new(Factory::default());
    let config = SessionRuntimeConfig {
        close_wait: Duration::from_millis(150),
        ..SessionRuntimeConfig::default()
    };
    let manager = SessionManager::new(factory.clone(), tokio::runtime::Handle::current(), config);
    SessionService::set_auto_reconnect_enabled(&manager, true)
        .await
        .expect("enable reconnect");
    let created = manager.create(create_request()).await.expect("open");
    let mut peer = factory.first_peer.lock().await.take().expect("first peer");
    peer.write_all(b"\x1b[6n").await.expect("cpr query");
    tokio::time::timeout(
        Duration::from_secs(2),
        factory.first_write_failed.notified(),
    )
    .await
    .expect("parser response write is attempted");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let sessions = manager
                .list(Default::default())
                .await
                .expect("session list");
            if sessions[0].lifecycle == SessionLifecycle::Failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unconfirmed failure closes the reader within the budget");
    assert_eq!(factory.exit_observed.load(Ordering::Acquire), 0);
    assert_eq!(factory.opens.load(Ordering::Acquire), 1);
    // 首次显式关闭仍会收到夹具注入的写端错误，重试必须能确认退役。
    drop(peer);
    assert!(manager.close(created.session_id.clone()).await.is_err());
    manager
        .close(created.session_id)
        .await
        .expect("retry close");
}
