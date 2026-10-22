//! 固定自动重连资格、有限退避和已排队计划失效语义。
//!
//! @author kongweiguang

#[path = "../src/sessions/auto_reconnect.rs"]
mod auto_reconnect;

use std::time::Duration;

use auto_reconnect::{
    AutoReconnectContext, AutoReconnectDecision, AutoReconnectPlan, AutoReconnectPolicy,
    AutoReconnectSuppression,
};
use kerminal_application::AppError;
use kerminal_domain::{
    identity::{AgentSessionId, HostId, PaneId, TabId},
    sessions::{
        SessionExitStatus, SessionOwner, SessionTarget, SshInitialCommand, SshSessionTarget,
        TelnetSessionTarget,
    },
};

/// 用产品实际 UI owner 形态构造策略输入，避免测试以任意布尔值代替 owner 类型。
///
/// @author kongweiguang
fn ui_owner() -> SessionOwner {
    SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    }
}

/// 使用结构化 SSH target 构造交互 shell 或显式初始命令场景。
///
/// @author kongweiguang
fn ssh_target(command: Option<SshInitialCommand>) -> SessionTarget {
    SessionTarget::Ssh(SshSessionTarget {
        host_id: HostId::new(),
        working_directory: None,
        command,
    })
}

/// 每次构造同一可重试断线事实，确保资格测试只改变要覆盖的输入维度。
///
/// @author kongweiguang
fn retryable_disconnect() -> AppError {
    AppError::retryable_remote_disconnect(std::io::Error::other("remote closed unexpectedly"))
}

/// 创建尚未关闭、无退出状态的普通 UI SSH 会话上下文。
///
/// @author kongweiguang
fn context<'a>(
    owner: &'a SessionOwner,
    target: &'a SessionTarget,
    failure: &'a AppError,
    attempts_started: u8,
) -> AutoReconnectContext<'a> {
    AutoReconnectContext {
        owner,
        target,
        failure,
        manually_closed: false,
        exit_status: None,
        attempts_started,
        session_generation: 23,
    }
}

/// 对唯一可重试场景核对指数退避、上限和最终尝试预算。
///
/// @author kongweiguang
#[test]
fn retries_only_ui_owned_interactive_ssh_with_bounded_backoff() {
    let policy = AutoReconnectPolicy::default();
    let owner = ui_owner();
    let target = ssh_target(None);
    let failure = retryable_disconnect();
    let expected_delays = [
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(10),
    ];

    for (attempts_started, expected_delay) in expected_delays.into_iter().enumerate() {
        let decision = policy.decide(
            true,
            context(&owner, &target, &failure, attempts_started as u8),
        );
        assert_eq!(
            decision,
            AutoReconnectDecision::Retry(AutoReconnectPlan {
                attempt: attempts_started as u8 + 1,
                delay: expected_delay,
                session_generation: 23,
            })
        );
    }

    assert_eq!(
        policy.decide(true, context(&owner, &target, &failure, 5)),
        AutoReconnectDecision::Suppressed(AutoReconnectSuppression::AttemptsExhausted)
    );
}

/// 后台 owner、非 SSH、执行过初始命令以及明确结束的会话不得被透明重放。
///
/// @author kongweiguang
#[test]
fn suppresses_non_ui_targets_initial_commands_manual_close_and_exit_status() {
    let policy = AutoReconnectPolicy::default();
    let target = ssh_target(None);
    let failure = retryable_disconnect();

    for owner in [
        SessionOwner::Mcp,
        SessionOwner::Agent {
            agent_session_id: AgentSessionId::new(),
        },
    ] {
        assert_eq!(
            policy.decide(true, context(&owner, &target, &failure, 0)),
            AutoReconnectDecision::Suppressed(AutoReconnectSuppression::NotUiOwned)
        );
    }

    let owner = ui_owner();
    let telnet = SessionTarget::Telnet(TelnetSessionTarget {
        host: "localhost".to_owned(),
        port: 23,
    });
    assert_eq!(
        policy.decide(true, context(&owner, &telnet, &failure, 0)),
        AutoReconnectDecision::Suppressed(AutoReconnectSuppression::NotSsh)
    );

    for command in [
        SshInitialCommand::Shell("deploy --apply".to_owned()),
        SshInitialCommand::Process {
            program: "git".to_owned(),
            args: vec!["push".to_owned()],
        },
    ] {
        let target = ssh_target(Some(command));
        assert_eq!(
            policy.decide(true, context(&owner, &target, &failure, 0)),
            AutoReconnectDecision::Suppressed(AutoReconnectSuppression::InitialCommand)
        );
    }

    let interactive_target = ssh_target(None);
    assert_eq!(
        policy.decide(
            true,
            AutoReconnectContext {
                manually_closed: true,
                ..context(&owner, &interactive_target, &failure, 0)
            },
        ),
        AutoReconnectDecision::Suppressed(AutoReconnectSuppression::ManualClose)
    );
    let exit_status = SessionExitStatus {
        code: Some(255),
        signal: None,
        success: false,
    };
    assert_eq!(
        policy.decide(
            true,
            AutoReconnectContext {
                exit_status: Some(&exit_status),
                ..context(&owner, &interactive_target, &failure, 0)
            },
        ),
        AutoReconnectDecision::Suppressed(AutoReconnectSuppression::HasExitStatus)
    );
}

/// 只有专用 retryable disconnect 能触发重试；认证、hostkey、超时和泛化 IO 仍失败关闭。
///
/// @author kongweiguang
#[test]
fn suppresses_authentication_hostkey_timeout_and_generic_failures() {
    let policy = AutoReconnectPolicy::default();
    let owner = ui_owner();
    let target = ssh_target(None);
    let failures = [
        AppError::AuthenticationFailed,
        AppError::HostKeyUnknown,
        AppError::HostKeyChanged,
        AppError::HostKeyTrustMismatch,
        AppError::timeout("SSH connect"),
        AppError::infrastructure("unclassified SSH IO failure"),
    ];

    for failure in failures {
        assert_eq!(
            policy.decide(true, context(&owner, &target, &failure, 0)),
            AutoReconnectDecision::Suppressed(AutoReconnectSuppression::NotRetryableDisconnect)
        );
    }
}

/// 配置关闭、显式取消或新 generation 都会使已排队 retry 计划失效。
///
/// @author kongweiguang
#[test]
fn invalidates_queued_plan_after_setting_change_cancel_or_generation_change() {
    let policy = AutoReconnectPolicy::default();
    let owner = ui_owner();
    let target = ssh_target(None);
    let failure = retryable_disconnect();
    let AutoReconnectDecision::Retry(plan) =
        policy.decide(true, context(&owner, &target, &failure, 0))
    else {
        panic!("eligible interactive SSH failure creates a retry plan");
    };

    assert_eq!(policy.validate_plan(plan, true, false, 23), Ok(()));
    assert_eq!(
        policy.validate_plan(plan, false, false, 23),
        Err(AutoReconnectSuppression::Disabled)
    );
    assert_eq!(
        policy.validate_plan(plan, true, true, 23),
        Err(AutoReconnectSuppression::Cancelled)
    );
    assert_eq!(
        policy.validate_plan(plan, true, false, 24),
        Err(AutoReconnectSuppression::GenerationChanged)
    );
}
