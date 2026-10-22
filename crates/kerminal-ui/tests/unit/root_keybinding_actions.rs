//! Root 标签循环快捷键在 UI 投影顺序上的边界回归。
//!
//! @author kongweiguang

use super::super::workspace_close::workspace_tab_activation_accepted;
use super::adjacent_workspace_tab_id;
use crate::features::{
    workspace_controller::WorkspaceControllerEffect,
    workspace_tabs::{
        WorkspaceTabEvent, WorkspaceTabKind, WorkspaceTabProjection,
        append_shared_sftp_workbench_projection,
    },
};
use gpui_kit::SharedString;
use kerminal_domain::identity::TabId;

/// 构造最小标签投影，保持测试覆盖顺序与身份而不引入 Root、文件或终端运行时。
///
/// @author kongweiguang
fn projection(id: &str, kind: WorkspaceTabKind, group_id: Option<&str>) -> WorkspaceTabProjection {
    WorkspaceTabProjection {
        id: TabId::parse(id).expect("valid tab id"),
        title: SharedString::from(id.to_owned()),
        group_id: group_id.map(str::to_owned),
        active: false,
        kind,
        layout: None,
    }
}

/// 混合类型沿用 Controller/UI 的顺序循环，未知标签与临时工作台都可抵达。
#[test]
fn adjacent_tab_cycle_uses_all_projected_kinds_and_wraps() {
    let tabs = vec![
        projection("terminal", WorkspaceTabKind::Terminal, None),
        projection("file", WorkspaceTabKind::WorkspaceFile, None),
        projection("transfer", WorkspaceTabKind::SftpTransfer, None),
        projection(
            "future",
            WorkspaceTabKind::Unsupported(SharedString::from("future")),
            None,
        ),
        projection("workbench", WorkspaceTabKind::SftpWorkbench, None),
    ];
    let ids = tabs.iter().map(|tab| tab.id.clone()).collect::<Vec<_>>();

    for index in 0..ids.len() {
        assert_eq!(
            adjacent_workspace_tab_id(&tabs, Some(&ids[index]), false),
            Some(ids[(index + 1) % ids.len()].clone())
        );
        assert_eq!(
            adjacent_workspace_tab_id(&tabs, Some(&ids[index]), true),
            Some(ids[(index + ids.len() - 1) % ids.len()].clone())
        );
    }
}

/// 无有效活动身份时从方向对应的一端进入，不依赖持久快照的 active 标记。
#[test]
fn adjacent_tab_cycle_starts_at_projection_edge_without_active_id() {
    let tabs = vec![
        projection("first", WorkspaceTabKind::WorkspaceFile, None),
        projection("last", WorkspaceTabKind::Terminal, None),
    ];

    assert_eq!(
        adjacent_workspace_tab_id(&tabs, None, false),
        Some(TabId::parse("first").unwrap())
    );
    assert_eq!(
        adjacent_workspace_tab_id(&tabs, None, true),
        Some(TabId::parse("last").unwrap())
    );
}

/// 临时工作台打开时仍在持久 active 后追加；被选中后以真实可见 ID 而非后台 active 继续循环。
#[test]
fn adjacent_tab_cycle_tracks_visible_workbench_selection() {
    let workbench_id = TabId::parse("workbench").unwrap();
    let mut persistent_active = vec![projection("terminal", WorkspaceTabKind::Terminal, None)];
    persistent_active[0].active = true;
    assert!(append_shared_sftp_workbench_projection(
        &mut persistent_active,
        &workbench_id,
        false,
    ));
    assert_eq!(
        adjacent_workspace_tab_id(
            &persistent_active,
            Some(&TabId::parse("terminal").unwrap()),
            false,
        ),
        Some(workbench_id.clone())
    );
    assert!(!persistent_active[1].active);

    let mut workbench_active = vec![projection("terminal", WorkspaceTabKind::Terminal, None)];
    workbench_active[0].active = true;
    assert!(append_shared_sftp_workbench_projection(
        &mut workbench_active,
        &workbench_id,
        true,
    ));
    assert!(!workbench_active[0].active);
    assert!(workbench_active[1].active);
    assert_eq!(
        adjacent_workspace_tab_id(&workbench_active, Some(&workbench_id), false),
        Some(TabId::parse("terminal").unwrap())
    );
}

/// 折叠组不从 Controller 投影中删除成员，隐藏的当前成员仍以原位置计算相邻标签。
#[test]
fn adjacent_tab_cycle_keeps_collapsed_group_members_in_projection_order() {
    let tabs = vec![
        projection("before", WorkspaceTabKind::Terminal, None),
        projection(
            "hidden-member",
            WorkspaceTabKind::WorkspaceFile,
            Some("collapsed"),
        ),
        projection("after", WorkspaceTabKind::SftpTransfer, None),
    ];

    assert_eq!(
        adjacent_workspace_tab_id(&tabs, Some(&TabId::parse("hidden-member").unwrap()), false,),
        Some(TabId::parse("after").unwrap())
    );
}

/// 空集合不产生伪目标，单标签返回同一身份供 Root 跳过重复激活副作用。
#[test]
fn adjacent_tab_cycle_handles_empty_and_single_projection() {
    assert_eq!(adjacent_workspace_tab_id(&[], None, false), None);

    let tabs = vec![projection("only", WorkspaceTabKind::SftpWorkbench, None)];
    let only = TabId::parse("only").unwrap();
    assert_eq!(
        adjacent_workspace_tab_id(&tabs, Some(&only), false),
        Some(only.clone())
    );
    assert_eq!(adjacent_workspace_tab_id(&tabs, None, true), Some(only));
}

/// reducer 拒绝或尚未就绪时不授权 Root 导航，避免可见页面绕过 controller 的活动标签状态。
///
/// @author kongweiguang
#[test]
fn rejected_activation_does_not_authorize_root_navigation() {
    let event = WorkspaceTabEvent::Activate(TabId::parse("target").unwrap());
    let rejected = Err::<WorkspaceControllerEffect, ()>(());

    assert!(!workspace_tab_activation_accepted(&event, Some(&rejected)));
    assert!(!workspace_tab_activation_accepted::<()>(&event, None));
}

/// Activate 返回 Unchanged 仍代表 reducer 已确认目标，Root 必须同步可见页；Changed 同样获准。
///
/// @author kongweiguang
#[test]
fn accepted_activation_authorizes_root_navigation_for_unchanged_and_changed() {
    let event = WorkspaceTabEvent::Activate(TabId::parse("target").unwrap());
    let unchanged = Ok::<WorkspaceControllerEffect, ()>(WorkspaceControllerEffect::Unchanged);
    let changed =
        Ok::<WorkspaceControllerEffect, ()>(WorkspaceControllerEffect::Changed { generation: 1 });

    assert!(workspace_tab_activation_accepted(&event, Some(&unchanged)));
    assert!(workspace_tab_activation_accepted(&event, Some(&changed)));
}
