//! 运维动作结果文案测试。
//!
//! @author kongweiguang

use super::{
    compose_config_endpoint, kill_session_result_message, tmux_mutation_feedback,
    tmux_session_creation_enabled,
};
use kerminal_application::{AppError, operations::TmuxCapability};
use kerminal_domain::{files::FileEndpoint, identity::HostId};

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
