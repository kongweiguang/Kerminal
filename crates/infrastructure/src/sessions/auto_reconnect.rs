//! 终端自动重连的无状态资格与退避策略；SessionManager 负责持有唯一运行态和取消任务。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_application::AppError;
use kerminal_domain::sessions::{SessionExitStatus, SessionOwner, SessionTarget};

const MAX_ATTEMPTS: u8 = 5;
const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(10);

/// 供 Manager 在每次断线时重新评估的纯策略；尝试计数与任务生命周期仍由唯一会话 owner 持有。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutoReconnectPolicy {
    max_attempts: u8,
    initial_delay: Duration,
    maximum_delay: Duration,
}

impl Default for AutoReconnectPolicy {
    /// 限制重试总量和指数退避上限，避免短暂断线造成持续连接风暴。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            initial_delay: INITIAL_DELAY,
            maximum_delay: MAX_DELAY,
        }
    }
}

/// 单次断线输入；generation 由现有 SessionManager incarnation 递增，不由策略另建状态。
///
/// @author kongweiguang
pub(crate) struct AutoReconnectContext<'a> {
    pub owner: &'a SessionOwner,
    pub target: &'a SessionTarget,
    pub failure: &'a AppError,
    pub manually_closed: bool,
    pub exit_status: Option<&'a SessionExitStatus>,
    pub attempts_started: u8,
    pub session_generation: u64,
}

/// 重连资格拒绝原因可用于内部诊断与策略测试，不映射为面向用户的新错误码。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutoReconnectSuppression {
    Disabled,
    Cancelled,
    GenerationChanged,
    NotUiOwned,
    NotSsh,
    InitialCommand,
    ManualClose,
    HasExitStatus,
    NotRetryableDisconnect,
    AttemptsExhausted,
}

/// 传给 Manager-owned sleeper 的不可变计划；过期 generation 不得启动新 transport。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutoReconnectPlan {
    pub attempt: u8,
    pub delay: Duration,
    pub session_generation: u64,
}

/// 资格通过时给出下一次有界尝试；策略本身不创建任务或 second session registry。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutoReconnectDecision {
    Retry(AutoReconnectPlan),
    Suppressed(AutoReconnectSuppression),
}

impl AutoReconnectPolicy {
    /// 仅允许 UI 拥有的交互式 SSH 会话因 typed disconnect 进入重试队列。
    ///
    /// 设置开关应取当前 `TerminalSettings.auto_reconnect`；每次重试前还需调用
    /// `validate_plan`，这样关闭设置或会话 generation 改变会使已排队计划失效。
    ///
    /// @author kongweiguang
    pub(crate) fn decide(
        self,
        setting_enabled: bool,
        context: AutoReconnectContext<'_>,
    ) -> AutoReconnectDecision {
        if !setting_enabled {
            return AutoReconnectDecision::Suppressed(AutoReconnectSuppression::Disabled);
        }
        if !matches!(context.owner, SessionOwner::Ui { .. }) {
            return AutoReconnectDecision::Suppressed(AutoReconnectSuppression::NotUiOwned);
        }
        match context.target {
            SessionTarget::Ssh(target) if target.command.is_none() => {}
            SessionTarget::Ssh(_) => {
                return AutoReconnectDecision::Suppressed(AutoReconnectSuppression::InitialCommand);
            }
            _ => {
                return AutoReconnectDecision::Suppressed(AutoReconnectSuppression::NotSsh);
            }
        }
        if context.manually_closed {
            return AutoReconnectDecision::Suppressed(AutoReconnectSuppression::ManualClose);
        }
        if context.exit_status.is_some() {
            return AutoReconnectDecision::Suppressed(AutoReconnectSuppression::HasExitStatus);
        }
        if !context.failure.is_retryable_remote_disconnect() {
            return AutoReconnectDecision::Suppressed(
                AutoReconnectSuppression::NotRetryableDisconnect,
            );
        }
        if context.attempts_started >= self.max_attempts {
            return AutoReconnectDecision::Suppressed(AutoReconnectSuppression::AttemptsExhausted);
        }

        let attempt = context.attempts_started + 1;
        AutoReconnectDecision::Retry(AutoReconnectPlan {
            attempt,
            delay: self.delay_for_attempt(attempt),
            session_generation: context.session_generation,
        })
    }

    /// 在 sleeper 醒来后复核实时设置、取消与 generation，避免旧目标或手动关闭后复活。
    ///
    /// @author kongweiguang
    pub(crate) fn validate_plan(
        self,
        plan: AutoReconnectPlan,
        setting_enabled: bool,
        cancelled: bool,
        current_generation: u64,
    ) -> Result<(), AutoReconnectSuppression> {
        if !setting_enabled {
            return Err(AutoReconnectSuppression::Disabled);
        }
        if cancelled {
            return Err(AutoReconnectSuppression::Cancelled);
        }
        if plan.session_generation != current_generation {
            return Err(AutoReconnectSuppression::GenerationChanged);
        }
        Ok(())
    }

    /// 按尝试序号指数增长并封顶，确保连续断线既有恢复机会也有有限重试节奏。
    ///
    /// @author kongweiguang
    fn delay_for_attempt(self, attempt: u8) -> Duration {
        let mut delay = self.initial_delay;
        for _ in 1..attempt {
            delay = delay.saturating_mul(2).min(self.maximum_delay);
        }
        delay
    }
}
