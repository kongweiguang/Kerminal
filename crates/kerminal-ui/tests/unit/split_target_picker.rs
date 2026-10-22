//! 分屏选择意图只依赖稳定身份和当前 typed 目标，不启动 GPUI window 或终端资源。
//!
//! @author kongweiguang

use kerminal_application::workspace::WorkspaceTargetRef;
use kerminal_domain::identity::{HostId, PaneId, SessionId, TabId};

use super::{SplitTargetPickerRequest, current_selected_option};
use crate::features::workspace_controller::WorkspaceSplitTargetOption;

/// 来源会话必须仍属于捕获的 pane/tab 且 Ready；相同 PaneId 的新 session 不接收旧点击。
#[test]
fn captured_source_rejects_stale_tab_pane_and_session() {
    let request = SplitTargetPickerRequest {
        tab_id: TabId::parse("tab-a").expect("valid tab ID"),
        pane_id: PaneId::parse("pane-a").expect("valid pane ID"),
        source_session_id: SessionId::new(),
        direction: kerminal_domain::workspace::SplitDirection::Horizontal,
        before: false,
        request_generation: 7,
    };

    assert!(request.matches_source(
        &request.tab_id,
        &request.pane_id,
        Some(&request.source_session_id),
        true,
    ));
    assert!(!request.matches_source(
        &TabId::parse("tab-b").expect("valid tab ID"),
        &request.pane_id,
        Some(&request.source_session_id),
        true,
    ));
    assert!(!request.matches_source(
        &request.tab_id,
        &request.pane_id,
        Some(&SessionId::new()),
        true,
    ));
    assert!(!request.matches_source(
        &request.tab_id,
        &request.pane_id,
        Some(&request.source_session_id),
        false,
    ));
}

/// 显示目录已变化时确认必须失效，不能把旧选项的行位置映射到另一个 host。
#[test]
fn confirmation_requires_the_same_current_target_reference() {
    let host_id = HostId::new();
    let selected = WorkspaceTargetRef::Ssh {
        host_id: host_id.clone(),
    };
    let option = WorkspaceSplitTargetOption {
        target: selected.clone(),
        title: "Build host".to_owned(),
        detail: Some("SSH · deploy@build.example:22".to_owned()),
        group_id: Some("group:dev".to_owned()),
    };
    let other = WorkspaceSplitTargetOption {
        target: WorkspaceTargetRef::Ssh {
            host_id: HostId::new(),
        },
        title: "Other host".to_owned(),
        detail: None,
        group_id: None,
    };

    assert_eq!(
        current_selected_option(std::slice::from_ref(&option), &selected),
        Some(option)
    );
    assert!(current_selected_option(&[other], &selected).is_none());
}
