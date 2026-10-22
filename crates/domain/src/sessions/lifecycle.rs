//! 会话生命周期的单一状态转移规则。
//!
//! @author kongweiguang

use std::fmt;

use super::SessionLifecycle;

/// 会话运行时可报告的生命周期事件。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleEvent {
    CloseRequested,
    CloseSucceeded,
    /// Close 清理失败后只恢复本次关闭前已结束的历史态。
    CloseFailedToHistory {
        previous: SessionLifecycle,
    },
    TransportExited,
    TransportFailed,
    ReconnectStarted,
    RestartStarted,
    ReconnectSucceeded,
    ReconnectFailed,
}

/// 拒绝非法状态转换时保留原状态和触发事件，供基础设施决定是否忽略过期回执。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLifecycleTransitionError {
    pub current: SessionLifecycle,
    pub event: SessionLifecycleEvent,
}

impl SessionLifecycle {
    /// 通过明确的领域事件推进生命周期，避免基础设施直接赋值绕开重连与关闭约束。
    ///
    /// Starting 同时代表启动、重连或主动重启中的状态；RestartStarted 单独允许运行态进入该状态，
    /// 不放宽 ReconnectStarted 的原有门禁。transport 回执只允许影响有效运行实例，因此 Closing
    /// 和 Closed 上的迟到事件会被拒绝，而不会覆盖关闭结果。
    ///
    /// @author kongweiguang
    pub const fn transition(
        self,
        event: SessionLifecycleEvent,
    ) -> Result<Self, SessionLifecycleTransitionError> {
        use SessionLifecycle::{Closed, Closing, Exited, Failed, Running, Starting};
        use SessionLifecycleEvent::{
            CloseFailedToHistory, CloseRequested, CloseSucceeded, ReconnectFailed,
            ReconnectStarted, ReconnectSucceeded, RestartStarted, TransportExited, TransportFailed,
        };

        let next = match (self, event) {
            (Starting | Running | Exited | Failed, CloseRequested) | (Closing, CloseRequested) => {
                Closing
            }
            (Closing, CloseSucceeded) => Closed,
            (Closing, CloseFailedToHistory { previous }) if matches!(previous, Exited | Failed) => {
                previous
            }
            (Running, TransportExited) => Exited,
            (Starting | Running, TransportFailed) => Failed,
            (Exited | Failed, ReconnectStarted) => Starting,
            (Running | Exited | Failed, RestartStarted) => Starting,
            (Starting, ReconnectSucceeded) => Running,
            (Starting, ReconnectFailed) => Failed,
            _ => {
                return Err(SessionLifecycleTransitionError {
                    current: self,
                    event,
                });
            }
        };

        Ok(next)
    }
}

impl fmt::Display for SessionLifecycleTransitionError {
    /// 输出不含会话标识或运行时数据的稳定诊断，避免领域错误泄露连接信息。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "会话生命周期 {:?} 不允许事件 {:?}",
            self.current, self.event
        )
    }
}

impl std::error::Error for SessionLifecycleTransitionError {}
