//! 左栏容器请求的目标和导航身份回归。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::sessions::{SshInitialCommand, SshSessionTarget};

/// 多个同宿主 pane 不能按遍历顺序猜选；活动 pane 是用户已明确选定的目标。
/// @author kongweiguang
#[test]
fn existing_host_pane_requires_unique_or_current_target() {
    let first = (TabId::new(), PaneId::new());
    let second = (TabId::new(), PaneId::new());
    assert_eq!(
        select_ssh_pane(Vec::new(), None, None),
        ExistingSshPane::None
    );
    assert_eq!(
        select_ssh_pane(vec![first.clone()], None, None),
        ExistingSshPane::One {
            tab_id: first.0.clone(),
            pane_id: first.1.clone()
        },
    );
    assert_eq!(
        select_ssh_pane(vec![first.clone(), second.clone()], None, None),
        ExistingSshPane::Multiple
    );
    assert_eq!(
        select_ssh_pane(
            vec![first, second.clone()],
            Some(&second.0),
            Some(&second.1)
        ),
        ExistingSshPane::One {
            tab_id: second.0,
            pane_id: second.1
        },
    );
}

/// 请求身份、导航代次与 SSH host 均要精确匹配，旧回执及单次命令不能续接容器面板。
/// @author kongweiguang
#[test]
fn preflight_rejects_stale_or_different_connection_intents() {
    let host_id = HostId::new();
    let other_host = HostId::new();
    let request = SidebarContainerRequest {
        id: "request-a".into(),
        host_id: host_id.clone(),
        pin: None,
        navigation_generation: 7,
        owner: None,
        tool_opened: false,
    };
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: None,
        command: None,
    });
    assert!(preflight_matches(
        &request,
        "request-a",
        &host_id,
        &target,
        7
    ));
    assert!(!preflight_matches(
        &request,
        "request-b",
        &host_id,
        &target,
        7
    ));
    assert!(!preflight_matches(
        &request,
        "request-a",
        &host_id,
        &target,
        8
    ));
    assert!(!preflight_matches(
        &request,
        "request-a",
        &other_host,
        &target,
        7
    ));
    let other_target = SessionTarget::Ssh(SshSessionTarget {
        host_id: other_host,
        working_directory: None,
        command: None,
    });
    assert!(!preflight_matches(
        &request,
        "request-a",
        &host_id,
        &other_target,
        7
    ));
    let command_target = SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: None,
        command: Some(SshInitialCommand::Shell("true".into())),
    });
    assert!(!preflight_matches(
        &request,
        "request-a",
        &host_id,
        &command_target,
        7
    ));
}
