//! 运维动作结果文案测试。
//!
//! @author kongweiguang

use super::super::container_mutation::ContainerOperationOutcome;
use super::{
    compose_config_endpoint, container_operation_error, container_operation_refresh_feedback,
    forward_delete_confirmation_current, forward_save_error_is_relevant,
    kill_session_result_message, new_forward_failure_message,
    report_forward_save_error_immediately, tmux_mutation_feedback, tmux_session_creation_enabled,
};
use kerminal_application::{AppError, operations::TmuxCapability};
use kerminal_domain::{files::FileEndpoint, identity::HostId};

/// 原表单仍在时可报告其在途错误；另一表单或已切走的工作区不能接收旧回执。
///
/// @author kongweiguang
#[test]
fn forward_save_errors_follow_original_form_or_active_target() {
    assert!(forward_save_error_is_relevant(true, true, false));
    assert!(!forward_save_error_is_relevant(false, true, true));
    assert!(forward_save_error_is_relevant(false, false, true));
    assert!(!forward_save_error_is_relevant(false, false, false));
}

/// 自动核对前不提示未知保存；确认失败和普通保存失败仍需要直接提示。
///
/// @author kongweiguang
#[test]
fn forward_unknown_save_waits_for_readback_before_feedback() {
    use kerminal_application::PublicErrorCode;
    assert!(!report_forward_save_error_immediately(
        PublicErrorCode::OutcomeUnknown
    ));
    assert!(report_forward_save_error_immediately(
        PublicErrorCode::Invalid
    ));
    assert!(report_forward_save_error_immediately(
        PublicErrorCode::Conflict
    ));
}

/// 隐藏确定成功反馈不能吞掉刷新失败或未知结果；后者仍需用户回读原会话。
#[test]
fn compact_tmux_feedback_keeps_unconfirmed_results_visible() {
    use kerminal_domain::operations::{TmuxMutationReceipt, TmuxSession};
    let receipt = TmuxMutationReceipt {
        session: TmuxSession {
            id: "$fixture".into(),
            name: "fixture".into(),
            attached: false,
            clients: 0,
            windows: 1,
            current_path: None,
            created_at: None,
            activity_at: None,
            target_ref: "local".into(),
        },
        refresh_required: false,
    };
    assert!(tmux_mutation_feedback(Ok(receipt.clone()), false, true).is_none());
    assert!(tmux_mutation_feedback(Ok(receipt.clone()), false, false).is_some());
    assert!(
        tmux_mutation_feedback(
            Ok(TmuxMutationReceipt {
                refresh_required: true,
                ..receipt
            }),
            false,
            true
        )
        .is_some()
    );
    let unknown = AppError::outcome_unknown("tmux result lost");
    let expected = unknown.public_message().to_owned();
    assert_eq!(
        tmux_mutation_feedback(Err(unknown), true, true).as_deref(),
        Some(expected.as_str())
    );
}

/// 只对目标主机中新进入 Failed 的隧道提示，重复清单读取不再次发出提示。
///
/// @author kongweiguang
#[test]
fn forward_failure_notice_is_scoped_and_emitted_only_on_transition() {
    use kerminal_application::operations::ManagedPortForwardSnapshot;
    use kerminal_domain::operations::{ForwardKind, ForwardState, PortForward};
    let host = HostId::parse("host-a").unwrap();
    let other_host = HostId::parse("host-b").unwrap();
    let config = super::super::forms::forward_config(
        Some(host.clone()),
        "test",
        ForwardKind::Local,
        "127.0.0.1",
        "1080",
        "127.0.0.1",
        "80",
    )
    .unwrap();
    let mut row = ManagedPortForwardSnapshot {
        forward: PortForward::starting("forward-a", config),
        persistent: true,
        actual_bind: None,
        proxy_url: None,
        proxy_application: None,
        failure_code: None,
    };
    let previous = row.clone();
    row.forward.state = ForwardState::Failed;
    assert!(
        new_forward_failure_message(Some(&[previous]), &[row.clone()], true, Some(&host)).is_some()
    );
    assert!(new_forward_failure_message(None, &[row.clone()], true, Some(&host)).is_some());
    assert!(
        new_forward_failure_message(Some(&[row.clone()]), &[row.clone()], true, Some(&host))
            .is_none()
    );
    assert!(new_forward_failure_message(None, &[row], true, Some(&other_host)).is_none());
}

/// 应用层明确返回未删除时，界面必须说明没有执行结束操作。
#[test]
fn missing_tmux_session_is_not_reported_as_killed() {
    let message = kill_session_result_message(Ok(false));

    assert!(message.contains("未执行") || message.contains("no kill was performed"));
    assert!(!message.contains("tmux session 已结束"));
}

/// 只有应用层确认删除后，界面才报告 session 已结束。
#[test]
fn killed_tmux_session_uses_success_result() {
    let message = kill_session_result_message(Ok(true));

    assert!(message.contains("已结束") || message.contains("session ended"));
}

/// 应用层错误保持其公共脱敏说明，不被 bool 分支转成成功。
#[test]
fn tmux_kill_error_is_preserved() {
    let message = kill_session_result_message(Err(AppError::unsupported("test")));

    assert!(!message.contains("session ended"));
    assert!(!message.contains("已结束"));
}

/// 创建动作必须同时有有效目标与正向能力探测；未知/不可用、busy 或无效目标均 fail closed。
///
/// @author kongweiguang
#[test]
fn tmux_create_requires_confirmed_available_capability() {
    let available = TmuxCapability {
        target_ref: "local".to_owned(),
        available: true,
        version: Some("tmux 3.4".to_owned()),
        reason: None,
    };
    let unavailable = TmuxCapability {
        available: false,
        version: None,
        reason: Some("tmux not found".to_owned()),
        ..available.clone()
    };

    assert!(tmux_session_creation_enabled(false, true, Some(&available)));
    assert!(!tmux_session_creation_enabled(
        false,
        true,
        Some(&unavailable)
    ));
    assert!(!tmux_session_creation_enabled(false, true, None));
    assert!(!tmux_session_creation_enabled(true, true, Some(&available)));
    assert!(!tmux_session_creation_enabled(
        false,
        false,
        Some(&available)
    ));
}

/// Local Compose 相对路径绑定到当前主机的原生绝对工作目录，再交给 FilesPane 预览。
///
/// 使用平台临时目录作为路径样本，避免在 Linux 上把 Windows 盘符字符串误认成绝对路径。
///
/// @author kongweiguang
#[test]
fn local_compose_config_resolves_against_absolute_working_directory() {
    let working_directory = std::env::temp_dir().join("kerminal-compose-project-test");
    assert!(working_directory.is_absolute());
    let expected_path = working_directory.join("compose.yaml");
    let working_directory = working_directory.to_string_lossy();
    let (endpoint, selection) =
        compose_config_endpoint(None, "compose.yaml", Some(working_directory.as_ref()))
            .expect("absolute local project directory resolves the config file");

    assert_eq!(selection, "compose.yaml");
    assert_eq!(
        endpoint,
        FileEndpoint::Local {
            path: expected_path,
        }
    );
}

/// Remote 根目录本身是有效的工作目录，不能因移除尾部斜线而误拒配置路径。
#[test]
fn remote_compose_config_resolves_from_root_directory() {
    let host = HostId::parse("compose-root-test").expect("valid host id");
    let (endpoint, selection) =
        compose_config_endpoint(Some(host.clone()), "compose.yaml", Some("/"))
            .expect("root directory resolves a relative compose path");

    assert_eq!(selection, "compose.yaml");
    assert_eq!(
        endpoint,
        FileEndpoint::Remote {
            host_id: host,
            path: kerminal_domain::files::RemotePath::parse("/compose.yaml")
                .expect("valid remote path"),
        }
    );
}

/// 没有可确认的本机或远端工作目录时，相对 Compose 路径不猜测预览目标。
#[test]
fn relative_compose_config_without_working_directory_is_rejected() {
    assert!(compose_config_endpoint(None, "compose.yaml", None).is_none());
    assert!(
        compose_config_endpoint(
            Some(HostId::parse("compose-relative-test").expect("valid host id")),
            "compose.yaml",
            Some("relative/project"),
        )
        .is_none()
    );
}

/// SSH 标签中出现 Windows 客户端路径时，不得将它偷偷解析到运行 GPUI 的本机文件。
///
/// @author kongweiguang
#[test]
fn remote_compose_windows_label_is_not_reinterpreted_as_local_file() {
    let host = HostId::new();
    assert!(
        compose_config_endpoint(
            Some(host),
            r"D:\projects\demo\compose.yaml",
            Some(r"D:\projects\demo"),
        )
        .is_none()
    );
}

/// 命令成功但刷新失败仍需恢复，未知回执在刷新后也不能显示成成功。
///
/// @author kongweiguang
#[test]
fn container_feedback_preserves_unknown_and_failed_refresh() {
    let unknown = container_operation_error(AppError::outcome_unknown("fixture unknown"), false);
    assert!(matches!(unknown, ContainerOperationOutcome::Unknown(_)));
    let (message, error) = container_operation_refresh_feedback(unknown, true);
    assert!(error);
    assert!(message.contains("fixture unknown"));
    let (_, error) =
        container_operation_refresh_feedback(ContainerOperationOutcome::Applied, false);
    assert!(error);
    let (_, error) = container_operation_refresh_feedback(ContainerOperationOutcome::Applied, true);
    assert!(!error);
}

/// 根确认必须匹配原代次和最新持久定义，修改、消失或换主机都不能继续删除。
///
/// @author kongweiguang
#[test]
fn forward_delete_confirmation_rejects_stale_target_and_definition() {
    use kerminal_application::operations::{ManagedPortForwardSnapshot, PortForwardDefinition};
    use kerminal_domain::operations::{ForwardKind, PortForward};
    let config = super::super::forms::forward_config(
        Some(HostId::new()),
        "fixture",
        ForwardKind::Local,
        "127.0.0.1",
        "15432",
        "localhost",
        "5432",
    )
    .unwrap();
    let definition = PortForwardDefinition {
        id: "forward-id".to_owned(),
        config: config.clone(),
    };
    let row = ManagedPortForwardSnapshot {
        forward: PortForward::starting("forward-id", config),
        persistent: true,
        actual_bind: None,
        proxy_url: None,
        proxy_application: None,
        failure_code: None,
    };
    let pending = (definition.clone(), 7);
    let ready = super::super::state::SectionPhase::Ready;
    assert!(forward_delete_confirmation_current(
        &definition,
        Some(&pending),
        7,
        ready,
        Some(std::slice::from_ref(&row))
    ));
    assert!(!forward_delete_confirmation_current(
        &definition,
        Some(&pending),
        8,
        ready,
        Some(std::slice::from_ref(&row))
    ));
    assert!(!forward_delete_confirmation_current(
        &definition,
        Some(&pending),
        7,
        ready,
        Some(&[])
    ));
    let mut changed = row;
    changed.forward.config.name = "changed".to_owned();
    assert!(!forward_delete_confirmation_current(
        &definition,
        Some(&pending),
        7,
        ready,
        Some(&[changed])
    ));
}
