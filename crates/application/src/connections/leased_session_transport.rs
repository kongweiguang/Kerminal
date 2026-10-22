//! 将一个 SSH session channel 与其独占连接租约绑定到同一关闭入口。
//!
//! @author kongweiguang

use std::sync::Arc;

use async_trait::async_trait;
use kerminal_domain::sessions::{SessionExitStatus, TerminalSize};
use tokio::time::Instant;

use super::RemoteConnectionLease;
use crate::{AppResult, sessions::SessionTransportControl};

/// 让 provider 的 PTY channel 和一次性 SSH lease 共用绝对 deadline 与清理 owner。
pub struct LeasedSessionTransportControl {
    channel: Arc<dyn SessionTransportControl>,
    lease: RemoteConnectionLease,
    close_reason: &'static str,
}

impl LeasedSessionTransportControl {
    /// 保留每次 PTY 创建得到的专属 lease，确保关闭一个 pane 不影响其它连接。
    ///
    /// @author kongweiguang
    pub fn new(
        channel: Arc<dyn SessionTransportControl>,
        lease: RemoteConnectionLease,
        close_reason: &'static str,
    ) -> Self {
        Self {
            channel,
            lease,
            close_reason,
        }
    }
}

#[async_trait]
impl SessionTransportControl for LeasedSessionTransportControl {
    /// resize 只转交给 PTY channel，不触碰其所属连接或其它 channel。
    ///
    /// @author kongweiguang
    async fn resize(&self, size: TerminalSize) -> AppResult<()> {
        self.channel.resize(size).await
    }

    /// 同一绝对期限依次清理 channel 和 parent lease；失败时保留双方 owner 供重试。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: Instant) -> AppResult<()> {
        let channel_result = self.channel.close_until(deadline).await;
        let lease_result = self.lease.close_until(deadline, self.close_reason).await;
        match (channel_result, lease_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(lease_error)) => Err(error.with_source(lease_error)),
        }
    }

    /// 退出状态来自当前 channel 的消息泵，连接关闭本身不伪造退出码。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        self.channel.wait_exit().await
    }
}
