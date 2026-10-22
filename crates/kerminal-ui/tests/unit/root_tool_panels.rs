//! Root ToolRail 与中心共享 Entity 的宿主去重规则测试。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use super::super::CenterPanel;
use super::{
    center_preserves_terminal_file_context, center_route_uses_tool_rail_entity,
    center_tool_panel_close_is_current, center_tool_panel_escape_target,
    center_tool_panel_focus_on_open, center_tool_placement_focus_is_current,
    center_tool_placement_focus_is_requested, compact_drawer_close_is_current,
    compact_drawer_escape_is_current, expanded_center_tool_panel, local_files_endpoint,
    ssh_files_endpoint, tool_rail_panel_is_active, tool_rail_panel_is_visible,
    ui_binding_matches_pane,
};
use crate::root::{
    keybinding_focus::classify_focus_scope,
    keybindings::KeybindingFocusScope,
    workbench::{
        ToolRailContextAction, ToolRailPanelSlots, ToolRailPanelState,
        compact_tool_panel_open_target, compact_tool_rail_panel_should_close,
    },
    workbench_layout::{WorkbenchLayoutInput, resolve_workbench_layout},
};
use kerminal_application::{
    preferences::{ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId},
    sessions::UiSessionBinding,
    workspace::WorkspaceTargetRef,
};
use kerminal_domain::{
    files::FileEndpoint,
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionOwner, SessionTarget, SshSessionTarget},
};

/// 生产切换分支仅在明确打开居中宿主时迁移焦点；普通 render、关闭和更高层 owner 都保留原输入控件。
///
/// @author kongweiguang
#[test]
fn center_tool_open_focus_requires_new_unblocked_center_owner() {
    let tool = ToolRailToolId::Snippets;
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(tool, ToolRailPanelPlacement::Center);
    let mut state = ToolRailPanelState::default();
    let before = state.project(&settings).panels;
    state.toggle(&settings, tool);
    let after = state.project(&settings).panels;
    assert!(center_tool_panel_focus_on_open(before, after, tool, false));
    assert!(!center_tool_panel_focus_on_open(before, after, tool, true));
    assert!(!center_tool_panel_focus_on_open(after, after, tool, false));
    assert!(!center_tool_panel_focus_on_open(after, before, tool, false));
    assert!(!center_tool_panel_focus_on_open(
        before,
        ToolRailPanelSlots {
            attached: Some(tool),
            ..ToolRailPanelSlots::default()
        },
        tool,
        false,
    ));
}

/// 原工具可见且非 Center 的显式变更才登记来源；受理失败、隐藏、未加载和其它停靠动作不产生焦点授权。
///
/// @author kongweiguang
#[test]
fn center_placement_focus_requires_accepted_visible_non_center_source() {
    let action = ToolRailContextAction::Dock(ToolRailPanelPlacement::Center);
    let previous = Some(ToolRailPanelPlacement::Attached);
    assert!(center_tool_placement_focus_is_requested(
        action, previous, true, true
    ));
    assert!(!center_tool_placement_focus_is_requested(
        action, previous, true, false
    ));
    assert!(!center_tool_placement_focus_is_requested(
        action, previous, false, true
    ));
    assert!(!center_tool_placement_focus_is_requested(
        action, None, true, true
    ));
    assert!(!center_tool_placement_focus_is_requested(
        action,
        Some(ToolRailPanelPlacement::Center),
        true,
        true,
    ));
    for action in [
        ToolRailContextAction::MoveUp,
        ToolRailContextAction::Dock(ToolRailPanelPlacement::Attached),
        ToolRailContextAction::Dock(ToolRailPanelPlacement::Left),
        ToolRailContextAction::Dock(ToolRailPanelPlacement::Bottom),
    ] {
        assert!(!center_tool_placement_focus_is_requested(
            action, previous, true, true
        ));
    }
}

/// 真实打开状态在配置回执投影时直接搬入 Center，无需 toggle；同一配置不能把关闭的工具凭空打开。
///
/// @author kongweiguang
#[test]
fn center_placement_reprojects_open_but_does_not_open_hidden_tool() {
    let tool = ToolRailToolId::Snippets;
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(tool, ToolRailPanelPlacement::Attached);
    let mut open = ToolRailPanelState::default();
    let mut closed = ToolRailPanelState::default();
    open.toggle(&settings, tool);
    assert_eq!(open.project(&settings).panels.attached, Some(tool));
    settings
        .panel_placements
        .insert(tool, ToolRailPanelPlacement::Center);
    assert_eq!(open.project(&settings).panels.center, Some(tool));
    assert_eq!(closed.project(&settings).panels.center, None);
}

/// 迟到成功只能消费原导航与焦点来源；替换 owner、未挂载 center 和上层模态都拒绝，不因 CAS 成功放宽。
///
/// @author kongweiguang
#[test]
fn center_placement_late_focus_revalidates_navigation_owner_and_mount() {
    assert!(center_tool_placement_focus_is_current(
        7, 7, true, true, true, false
    ));
    for facts in [
        (7, 8, true, true, true, false),
        (7, 7, false, true, true, false),
        (7, 7, true, false, true, false),
        (7, 7, true, true, false, false),
        (7, 7, true, true, true, true),
    ] {
        assert!(!center_tool_placement_focus_is_current(
            facts.0, facts.1, facts.2, facts.3, facts.4, facts.5,
        ));
    }
}

/// 键盘与点击共用真实布局的 compact 活动工具；窄窗先打开/再关闭抽屉，展开布局仍保留原 toggle 关闭语义。
///
/// @author kongweiguang
#[test]
fn compact_tool_shortcut_uses_rendered_active_host_and_preserves_expanded_toggle() {
    let tool = ToolRailToolId::Snippets;
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(tool, ToolRailPanelPlacement::Center);
    let mut state = ToolRailPanelState::default();
    state.toggle(&settings, tool);
    assert_eq!(state.project(&settings).panels.center, Some(tool));
    let tools = [tool];
    for width in [300.0, 1800.0] {
        let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
            viewport_width: width,
            viewport_height: 900.0,
            host_sidebar_collapsed: false,
            right_dock_collapsed: false,
            host_sidebar_preferred_width: 220.0,
            attached_dock_preferred_width: 420.0,
            left_tool_open: false,
            attached_tool_open: false,
            bottom_tool_open: false,
            focused_tools_height: 0.0,
            focused_tools_expanded_height: 0.0,
            status_bar_height: 26.0,
            reserved_vertical_height: 0.0,
            open_tool_order: &tools,
        });
        if width == 300.0 {
            assert!(plan.compact_panels);
            assert_eq!(plan.compact_active_tool, Some(tool));
            assert!(!compact_tool_rail_panel_should_close(
                false,
                plan.compact_active_tool,
                tool
            ));
            assert!(compact_tool_rail_panel_should_close(
                true,
                plan.compact_active_tool,
                tool
            ));
            assert!(!compact_tool_rail_panel_should_close(
                true,
                plan.compact_active_tool,
                ToolRailToolId::Logs,
            ));
            assert_eq!(
                expanded_center_tool_panel(state.project(&settings).panels, true),
                None
            );
        } else {
            assert!(!plan.compact_panels);
            assert_eq!(plan.compact_active_tool, None);
            state.toggle(&settings, tool);
            assert_eq!(state.project(&settings).panels.center, None);
        }
    }
}

/// 832px 无面板时可用展开布局，首次打开后才转为 compact；必须同次补齐宿主，不能等待第二次图标点击。
///
/// @author kongweiguang
#[test]
fn first_agent_open_reprojects_832px_layout_before_choosing_its_host() {
    let tool = ToolRailToolId::AgentLauncher;
    let tools = [tool];
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(tool, ToolRailPanelPlacement::Attached);
    let saved_placements = settings.panel_placements.clone();
    let mut state = ToolRailPanelState::default();
    let mut input = WorkbenchLayoutInput {
        viewport_width: 832.0,
        viewport_height: 833.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: false,
        host_sidebar_preferred_width: 224.0,
        attached_dock_preferred_width: 324.0,
        left_tool_open: false,
        attached_tool_open: false,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 26.0,
        reserved_vertical_height: 0.0,
        open_tool_order: &[],
    };
    let before = resolve_workbench_layout(&input);
    assert!(!before.compact_panels);
    assert!(!before.effective_host_sidebar_collapsed);

    state.toggle(&settings, tool);
    let panels = state.project(&settings).panels;
    input.attached_tool_open = panels.attached.is_some();
    input.open_tool_order = &tools;
    let after = resolve_workbench_layout(&input);
    assert!(after.compact_panels);
    assert!(after.effective_host_sidebar_collapsed);
    assert_eq!(
        compact_tool_panel_open_target(tool, panels, after),
        Some(tool)
    );
    assert_eq!(
        state.project(&settings).panels,
        panels,
        "host selection must not toggle a second time"
    );
    assert_eq!(settings.panel_placements, saved_placements);

    assert!(compact_tool_rail_panel_should_close(
        true,
        after.compact_active_tool,
        tool
    ));
    assert!(state.close(&settings, tool));
    assert_eq!(
        compact_tool_panel_open_target(tool, state.project(&settings).panels, after),
        None
    );
}

/// 后投影覆盖宽度与底部高度变化；仍能正常展开或已被关闭的工具不能误挂 compact 隐形宿主。
///
/// @author kongweiguang
#[test]
fn first_tool_open_uses_current_placement_geometry_and_never_reopens_a_closed_owner() {
    let tool = ToolRailToolId::AgentLauncher;
    let tools = [tool];
    for (placement, width, height, expected_compact) in [
        (ToolRailPanelPlacement::Attached, 1800.0, 833.0, false),
        (ToolRailPanelPlacement::Center, 832.0, 833.0, false),
        (ToolRailPanelPlacement::Left, 832.0, 833.0, true),
        (ToolRailPanelPlacement::Bottom, 1800.0, 400.0, true),
    ] {
        let mut settings = ToolRailSettings::default();
        settings.panel_placements.insert(tool, placement);
        let mut state = ToolRailPanelState::default();
        state.toggle(&settings, tool);
        let panels = state.project(&settings).panels;
        let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
            viewport_width: width,
            viewport_height: height,
            host_sidebar_collapsed: false,
            right_dock_collapsed: false,
            host_sidebar_preferred_width: 224.0,
            attached_dock_preferred_width: 324.0,
            left_tool_open: panels.left.is_some(),
            attached_tool_open: panels.attached.is_some(),
            bottom_tool_open: panels.bottom.is_some(),
            focused_tools_height: 0.0,
            focused_tools_expanded_height: 0.0,
            status_bar_height: 26.0,
            reserved_vertical_height: 0.0,
            open_tool_order: &tools,
        });
        assert_eq!(plan.compact_panels, expected_compact);
        assert_eq!(
            compact_tool_panel_open_target(tool, panels, plan),
            expected_compact.then_some(tool)
        );
        assert_eq!(
            compact_tool_panel_open_target(ToolRailToolId::Logs, panels, plan),
            None
        );
        assert!(state.close(&settings, tool));
        assert_eq!(
            compact_tool_panel_open_target(tool, state.project(&settings).panels, plan),
            None
        );
    }
}

/// 新展开原 Dock 时可能还有其它打开项排在最后；首次激活必须固定请求工具，不转投 last-open owner。
///
/// @author kongweiguang
#[test]
fn compact_first_open_keeps_requested_owner_when_another_tool_was_last_open() {
    let tool = ToolRailToolId::AgentLauncher;
    let other = ToolRailToolId::Logs;
    let tools = [tool, other];
    let panels = ToolRailPanelSlots {
        attached: Some(tool),
        center: Some(other),
        ..ToolRailPanelSlots::default()
    };
    let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
        viewport_width: 832.0,
        viewport_height: 833.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: false,
        host_sidebar_preferred_width: 224.0,
        attached_dock_preferred_width: 424.0,
        left_tool_open: false,
        attached_tool_open: true,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 26.0,
        reserved_vertical_height: 0.0,
        open_tool_order: &tools,
    });
    assert_eq!(plan.compact_active_tool, Some(other));
    assert_eq!(
        compact_tool_panel_open_target(tool, panels, plan),
        Some(tool)
    );
}

/// 使用真实窄布局投影允许 System 宿主关闭；关闭与位置无关，不借 toggle 改写原工具的持久意图。
///
/// @author kongweiguang
#[test]
fn compact_tool_escape_requires_current_rendered_owner_and_surface() {
    let tool = ToolRailToolId::System;
    let tools = [tool];
    let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
        viewport_width: 602.0,
        viewport_height: 900.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: false,
        host_sidebar_preferred_width: 220.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: false,
        attached_tool_open: true,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 26.0,
        reserved_vertical_height: 0.0,
        open_tool_order: &tools,
    });
    let projection = (plan.compact_panels, plan.compact_active_tool);
    assert!(compact_drawer_escape_is_current(
        "escape",
        Some(tool),
        projection,
        (false, true),
        true,
        false,
    ));
    for (key, projection, open, focused, blocked) in [
        ("enter", projection, (false, true), true, false),
        ("escape", (false, Some(tool)), (false, true), true, false),
        (
            "escape",
            (true, Some(ToolRailToolId::Sftp)),
            (false, true),
            true,
            false,
        ),
        ("escape", (true, None), (false, true), true, false),
        ("escape", projection, (false, false), true, false),
        ("escape", projection, (true, false), true, false),
        ("escape", projection, (true, true), true, false),
        ("escape", projection, (false, true), false, false),
        ("escape", projection, (false, true), true, true),
    ] {
        assert!(!compact_drawer_escape_is_current(
            key,
            Some(tool),
            projection,
            open,
            focused,
            blocked,
        ));
    }
    for context in ["Modal", "PopupMenu", "Popover"] {
        let contexts = [gpui_kit::KeyContext::parse(context).expect("valid context")];
        let blocked = classify_focus_scope(&contexts, false) == KeybindingFocusScope::Modal;
        assert!(blocked);
        assert!(!compact_drawer_escape_is_current(
            "escape",
            Some(tool),
            projection,
            (false, true),
            true,
            blocked,
        ));
    }
}

/// 主机身份不能关闭工具抽屉；重复 Escape、恢复宽布局和主机搜索/菜单的上层 owner 均不误关。
///
/// @author kongweiguang
#[test]
fn compact_host_escape_does_not_retarget_tools_or_closed_drawer() {
    let projection = (true, Some(ToolRailToolId::System));
    assert!(compact_drawer_escape_is_current(
        "escape",
        None,
        projection,
        (true, false),
        true,
        false,
    ));
    for (projection, open, focused, blocked) in [
        (projection, (false, true), true, false),
        (projection, (false, false), true, false),
        (projection, (true, true), true, false),
        ((false, None), (true, false), true, false),
        (projection, (true, false), false, false),
        (projection, (true, false), true, true),
    ] {
        assert!(!compact_drawer_escape_is_current(
            "escape", None, projection, open, focused, blocked,
        ));
    }
}

/// X 与遮罩共用真实 compact 身份；旧工具、主机/工具交叉、自动展开和上层确认不能借点击关闭新宿主。
///
/// @author kongweiguang
#[test]
fn compact_pointer_close_requires_current_owner_and_unblocked_surface() {
    let tool = ToolRailToolId::Sftp;
    let projection = (true, Some(tool));
    assert!(compact_drawer_close_is_current(
        Some(tool),
        projection,
        (false, true),
        false,
    ));
    assert!(compact_drawer_close_is_current(
        None,
        projection,
        (true, false),
        false,
    ));
    for (owner, projection, open, blocked) in [
        (Some(tool), projection, (false, false), false),
        (Some(tool), projection, (true, false), false),
        (Some(tool), projection, (true, true), false),
        (Some(ToolRailToolId::Logs), projection, (false, true), false),
        (None, projection, (false, true), false),
        (Some(tool), (false, Some(tool)), (false, true), false),
        (Some(tool), projection, (false, true), true),
        (None, projection, (true, false), true),
    ] {
        assert!(!compact_drawer_close_is_current(
            owner, projection, open, blocked
        ));
    }
}

/// 显式关闭只移除精确工具的打开事实，保留其它宿主、保存位置和已拖动宽度；重复关闭不得变为打开。
///
/// @author kongweiguang
#[test]
fn explicit_compact_close_preserves_placement_width_and_other_runtime_tools() {
    let tool = ToolRailToolId::Snippets;
    let other = ToolRailToolId::Logs;
    for placement in [
        ToolRailPanelPlacement::Attached,
        ToolRailPanelPlacement::Left,
        ToolRailPanelPlacement::Bottom,
        ToolRailPanelPlacement::Center,
    ] {
        let mut settings = ToolRailSettings::default();
        settings.panel_placements.insert(tool, placement);
        settings.panel_placements.insert(
            other,
            if placement == ToolRailPanelPlacement::Left {
                ToolRailPanelPlacement::Bottom
            } else {
                ToolRailPanelPlacement::Left
            },
        );
        let saved_placements = settings.panel_placements.clone();
        let mut state = ToolRailPanelState::default();
        state.restore_attached_widths(&BTreeMap::from([(tool, 384)]));
        state.toggle(&settings, other);
        state.toggle(&settings, tool);
        let original_width = state.attached_width(tool);
        assert!(original_width.is_some());

        assert!(state.close(&settings, tool));
        assert!(!state.close(&settings, tool));
        let panels = state.project(&settings).panels;
        let remaining = [panels.attached, panels.left, panels.bottom, panels.center];
        assert!(!remaining.contains(&Some(tool)));
        assert!(remaining.contains(&Some(other)));
        assert_eq!(settings.panel_placements, saved_placements);
        assert_eq!(state.attached_width(tool), original_width);
        assert_eq!(expanded_center_tool_panel(panels, false), None);
    }
}

/// 只有显式 close 才移除运行时工具；连续窄/宽响应式投影不能改保存位置、打开 owner 或宽度。
///
/// @author kongweiguang
#[test]
fn responsive_projection_keeps_open_center_owner_until_explicit_close() {
    let tool = ToolRailToolId::Snippets;
    let tools = [tool];
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(tool, ToolRailPanelPlacement::Center);
    let mut state = ToolRailPanelState::default();
    state.toggle(&settings, tool);
    state.restore_attached_widths(&BTreeMap::from([(tool, 384)]));
    let original_width = state.attached_width(tool);
    for width in [602.0, 1920.0, 602.0, 1920.0] {
        let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
            viewport_width: width,
            viewport_height: 900.0,
            host_sidebar_collapsed: false,
            right_dock_collapsed: false,
            host_sidebar_preferred_width: 220.0,
            attached_dock_preferred_width: 420.0,
            left_tool_open: false,
            attached_tool_open: false,
            bottom_tool_open: false,
            focused_tools_height: 0.0,
            focused_tools_expanded_height: 0.0,
            status_bar_height: 26.0,
            reserved_vertical_height: 0.0,
            open_tool_order: &tools,
        });
        assert_eq!(plan.compact_panels, width == 602.0);
        let panels = state.project(&settings).panels;
        assert_eq!(panels.center, Some(tool));
        assert_eq!(
            expanded_center_tool_panel(panels, plan.compact_panels),
            if plan.compact_panels {
                None
            } else {
                Some(tool)
            }
        );
        assert_eq!(state.attached_width(tool), original_width);
        assert_eq!(
            settings.panel_placements.get(&tool),
            Some(&ToolRailPanelPlacement::Center)
        );
    }
    assert!(state.close(&settings, tool));
    assert_eq!(
        expanded_center_tool_panel(state.project(&settings).panels, false),
        None
    );
}

/// Center 的 X/遮罩不依赖路径输入让出 Escape，但仍拒绝旧 owner、紧凑投影与菜单/模态 fence。
///
/// @author kongweiguang
#[test]
fn center_pointer_close_remains_available_without_bypassing_owner_or_modal_fences() {
    let tool = ToolRailToolId::Snippets;
    let panels = ToolRailPanelSlots {
        center: Some(tool),
        ..ToolRailPanelSlots::default()
    };
    assert!(center_tool_panel_close_is_current(
        tool, panels, false, false
    ));
    assert_eq!(
        center_tool_panel_escape_target("escape", tool, panels, false, false),
        None
    );
    assert!(!center_tool_panel_close_is_current(
        tool, panels, true, false
    ));
    assert!(!center_tool_panel_close_is_current(
        ToolRailToolId::Logs,
        panels,
        false,
        false
    ));
    assert!(!center_tool_panel_close_is_current(
        tool,
        ToolRailPanelSlots::default(),
        false,
        false
    ));
    for context in ["Modal", "PopupMenu", "Popover"] {
        let contexts = [gpui_kit::KeyContext::parse(context).expect("valid context")];
        let blocked = classify_focus_scope(&contexts, false) == KeybindingFocusScope::Modal;
        assert!(blocked);
        assert!(!center_tool_panel_close_is_current(
            tool, panels, false, blocked
        ));
    }
}

/// Escape 仅关闭当前居中且仍有物理焦点的原工具；菜单/模态、失焦、旧回调和重复键不转投其它面板。
///
/// @author kongweiguang
#[test]
fn center_tool_escape_revalidates_owner_focus_and_upper_surface() {
    let tool = ToolRailToolId::Snippets;
    let replacement = ToolRailToolId::Logs;
    let mut settings = ToolRailSettings::default();
    for candidate in [tool, replacement] {
        settings
            .panel_placements
            .insert(candidate, ToolRailPanelPlacement::Center);
    }
    let mut state = ToolRailPanelState::default();
    state.toggle(&settings, tool);
    let panels = state.project(&settings).panels;
    assert_eq!(
        center_tool_panel_escape_target("escape", tool, panels, true, false),
        Some(tool)
    );
    for (key, focus_in_surface, blocked) in [("enter", true, false), ("escape", false, false)] {
        assert_eq!(
            center_tool_panel_escape_target(key, tool, panels, focus_in_surface, blocked),
            None
        );
    }
    for context in ["Modal", "PopupMenu", "Popover"] {
        let contexts = [gpui_kit::KeyContext::parse(context).expect("valid context")];
        let blocked = classify_focus_scope(&contexts, false) == KeybindingFocusScope::Modal;
        assert!(blocked);
        assert_eq!(
            center_tool_panel_escape_target("escape", tool, panels, true, blocked),
            None
        );
    }
    let accepted = center_tool_panel_escape_target("escape", tool, panels, true, false)
        .expect("current center owns the unconsumed Escape");
    state.toggle(&settings, accepted);
    assert_eq!(
        center_tool_panel_escape_target(
            "escape",
            tool,
            state.project(&settings).panels,
            true,
            false,
        ),
        None,
        "a repeat/stale callback cannot reopen the hidden trap"
    );
    state.toggle(&settings, replacement);
    assert_eq!(
        center_tool_panel_escape_target(
            "escape",
            tool,
            state.project(&settings).panels,
            true,
            false,
        ),
        None,
        "the original callback cannot close a replacement tool"
    );
}

/// 使用真实响应式预算验证共享临时句柄不会同时挂到 center 与任一 compact 抽屉，而非靠注释假设互斥。
///
/// @author kongweiguang
#[test]
fn center_and_compact_drawers_have_mutually_exclusive_focus_hosts() {
    let panels = ToolRailPanelSlots {
        center: Some(ToolRailToolId::Snippets),
        ..ToolRailPanelSlots::default()
    };
    let tools = [ToolRailToolId::Snippets];
    let mut saw_expanded = false;
    let mut saw_compact = false;
    for width in [300.0, 800.0, 1800.0] {
        for height in [220.0, 900.0] {
            let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
                viewport_width: width,
                viewport_height: height,
                host_sidebar_collapsed: false,
                right_dock_collapsed: false,
                host_sidebar_preferred_width: 220.0,
                attached_dock_preferred_width: 420.0,
                left_tool_open: true,
                attached_tool_open: true,
                bottom_tool_open: true,
                focused_tools_height: 0.0,
                focused_tools_expanded_height: 0.0,
                status_bar_height: 26.0,
                reserved_vertical_height: 0.0,
                open_tool_order: &tools,
            });
            let expanded = expanded_center_tool_panel(panels, plan.compact_panels);
            assert!(!(plan.compact_width && expanded.is_some()));
            assert!(!(plan.compact_panels && expanded.is_some()));
            if plan.compact_panels {
                saw_compact = true;
                assert_eq!(expanded, None);
            } else {
                saw_expanded = true;
                assert_eq!(expanded, Some(ToolRailToolId::Snippets));
            }
        }
    }
    assert!(saw_expanded && saw_compact);
}

/// 中心内容仅在可见 ToolRail 实际复用同一业务 Entity 时让出唯一宿主。
///
/// @author kongweiguang
#[test]
fn center_route_yields_only_to_visible_shared_tool_entity() {
    assert!(center_route_uses_tool_rail_entity(
        CenterPanel::Commands,
        true,
        true,
    ));
    assert!(!center_route_uses_tool_rail_entity(
        CenterPanel::Commands,
        true,
        false,
    ));
    assert!(center_route_uses_tool_rail_entity(
        CenterPanel::Files,
        true,
        true,
    ));
    assert!(!center_route_uses_tool_rail_entity(
        CenterPanel::Files,
        false,
        true,
    ));
    assert!(!center_route_uses_tool_rail_entity(
        CenterPanel::Terminal,
        true,
        true,
    ));
}

/// 中央编辑器切换不释放其来源 terminal pane；其余中心内容不凭旧焦点延续文件目标。
///
/// @author kongweiguang
#[test]
fn editor_center_preserves_only_the_current_terminal_file_context() {
    assert!(center_preserves_terminal_file_context(
        CenterPanel::Terminal
    ));
    assert!(center_preserves_terminal_file_context(CenterPanel::Editor));
    assert!(!center_preserves_terminal_file_context(
        CenterPanel::Commands
    ));
    assert!(!center_preserves_terminal_file_context(CenterPanel::Files));
    assert!(!center_preserves_terminal_file_context(
        CenterPanel::Connections
    ));
}

/// 隐藏或折叠的固定分区必须停止轮询，但其独立 Entity 仍可由 Root 保留。
///
/// @author kongweiguang
#[test]
fn activity_tracks_the_actual_unique_panel_slot() {
    let attached = ToolRailPanelSlots {
        attached: Some(ToolRailToolId::System),
        ..ToolRailPanelSlots::default()
    };
    assert!(tool_rail_panel_is_visible(
        attached,
        false,
        ToolRailToolId::System,
    ));
    assert!(!tool_rail_panel_is_visible(
        attached,
        true,
        ToolRailToolId::System,
    ));
    assert!(!tool_rail_panel_is_visible(
        attached,
        false,
        ToolRailToolId::Tmux,
    ));

    let bottom = ToolRailPanelSlots {
        bottom: Some(ToolRailToolId::Ports),
        ..ToolRailPanelSlots::default()
    };
    assert!(tool_rail_panel_is_visible(
        bottom,
        true,
        ToolRailToolId::Ports,
    ));

    let agent = ToolRailPanelSlots {
        attached: Some(ToolRailToolId::AgentLauncher),
        ..ToolRailPanelSlots::default()
    };
    assert!(tool_rail_panel_is_visible(
        agent,
        false,
        ToolRailToolId::AgentLauncher,
    ));
    assert!(!tool_rail_panel_is_visible(
        agent,
        true,
        ToolRailToolId::AgentLauncher,
    ));
}

/// 窗口失焦时所有宿主暂停采样，恢复聚焦后仅激活真实挂载的工具。
///
/// @author kongweiguang
#[test]
fn activity_requires_window_focus_and_a_visible_host() {
    let panels = ToolRailPanelSlots {
        bottom: Some(ToolRailToolId::System),
        ..ToolRailPanelSlots::default()
    };
    assert!(!tool_rail_panel_is_active(
        panels,
        false,
        false,
        ToolRailToolId::System,
    ));
    assert!(tool_rail_panel_is_active(
        panels,
        false,
        true,
        ToolRailToolId::System,
    ));
    assert!(!tool_rail_panel_is_active(
        panels,
        false,
        true,
        ToolRailToolId::Ports,
    ));
}

/// 活动 UI-owned SSH 目标按当前目录优先生成远端 endpoint，目标不一致时拒绝绑定。
///
/// @author kongweiguang
#[test]
fn active_ssh_files_target_uses_matching_workspace_target_and_current_directory() {
    let host_id = HostId::new();
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: Some(PathBuf::from("/configured")),
        command: None,
    });
    let persisted_target = WorkspaceTargetRef::Ssh {
        host_id: host_id.clone(),
    };

    let endpoint = ssh_files_endpoint(
        &target,
        Some(&persisted_target),
        Some(&host_id),
        Some("/current"),
        Some("/previous"),
    )
    .expect("matching active SSH should produce a files endpoint");
    assert_eq!(
        endpoint.remote_path().map(|path| path.as_str()),
        Some("/current")
    );

    let fallback_endpoint = ssh_files_endpoint(
        &target,
        Some(&persisted_target),
        None,
        None,
        Some("/previous"),
    )
    .expect("workspace cwd should be the next SSH directory fallback");
    assert_eq!(
        fallback_endpoint.remote_path().map(|path| path.as_str()),
        Some("/previous")
    );

    assert!(
        ssh_files_endpoint(
            &target,
            Some(&persisted_target),
            Some(&HostId::new()),
            Some("/current"),
            None,
        )
        .is_none()
    );
    assert!(
        ssh_files_endpoint(
            &target,
            Some(&WorkspaceTargetRef::Ssh {
                host_id: HostId::new(),
            }),
            None,
            Some("/current"),
            None,
        )
        .is_none()
    );
}

/// 为本机文件目标准备不访问进程环境的 typed session target。
///
/// @author kongweiguang
fn local_session_target(working_directory: Option<&str>) -> SessionTarget {
    SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("pwsh.exe"),
        args: Vec::new(),
        working_directory: working_directory.map(PathBuf::from),
        environment: BTreeMap::new(),
    })
}

/// 本机文件目标优先使用当前/持久 CWD，再回退到 Profile 与显式本机起始目录，不猜宿主当前目录。
///
/// @author kongweiguang
#[test]
fn local_files_endpoint_prefers_current_path_and_requires_matching_local_target() {
    let target = local_session_target(Some("C:\\profile-start"));
    let local_target = WorkspaceTargetRef::Local {
        profile_id: Some("profile-a".to_owned()),
    };
    let initial_local = FileEndpoint::Local {
        path: PathBuf::from("C:\\Users\\24052"),
    };

    assert_eq!(
        local_files_endpoint(
            &target,
            Some(&local_target),
            None,
            Some("profile-a"),
            Some("C:\\session-current"),
            Some("C:\\saved-cwd"),
            Some(&initial_local),
        ),
        Some(FileEndpoint::Local {
            path: PathBuf::from("C:\\session-current"),
        })
    );
    assert_eq!(
        local_files_endpoint(
            &target,
            Some(&local_target),
            None,
            Some("profile-a"),
            None,
            Some("C:\\saved-cwd"),
            Some(&initial_local),
        ),
        Some(FileEndpoint::Local {
            path: PathBuf::from("C:\\saved-cwd"),
        })
    );
    assert_eq!(
        local_files_endpoint(
            &target,
            Some(&local_target),
            None,
            Some("profile-a"),
            None,
            None,
            Some(&initial_local),
        ),
        Some(FileEndpoint::Local {
            path: PathBuf::from("C:\\profile-start"),
        })
    );

    let target_without_directory = local_session_target(None);
    assert_eq!(
        local_files_endpoint(
            &target_without_directory,
            Some(&WorkspaceTargetRef::Local { profile_id: None }),
            None,
            None,
            None,
            None,
            Some(&initial_local),
        ),
        Some(initial_local.clone())
    );
    assert!(
        local_files_endpoint(
            &target_without_directory,
            Some(&WorkspaceTargetRef::Local { profile_id: None }),
            None,
            None,
            None,
            None,
            None,
        )
        .is_none()
    );
    assert!(
        local_files_endpoint(
            &target_without_directory,
            Some(&WorkspaceTargetRef::Ssh {
                host_id: HostId::new(),
            }),
            None,
            None,
            None,
            None,
            Some(&initial_local),
        )
        .is_none()
    );
    assert!(
        local_files_endpoint(
            &target_without_directory,
            Some(&local_target),
            None,
            Some("different-profile"),
            None,
            None,
            Some(&initial_local),
        )
        .is_none()
    );
    assert!(
        local_files_endpoint(
            &target_without_directory,
            Some(&WorkspaceTargetRef::Local { profile_id: None }),
            Some(&HostId::new()),
            None,
            None,
            None,
            Some(&initial_local),
        )
        .is_none()
    );
}

/// 文件目标只从与实际 tab、pane、session 完全一致的 UI binding 接受。
///
/// @author kongweiguang
#[test]
fn file_target_requires_the_exact_ui_session_binding() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
        session_generation: 3,
    };

    assert!(ui_binding_matches_pane(
        &binding,
        &session_id,
        &tab_id,
        &pane_id,
    ));
    assert!(!ui_binding_matches_pane(
        &binding,
        &session_id,
        &TabId::new(),
        &pane_id,
    ));
}

/// 缺失或不匹配的持久化 SSH host 不允许从活动终端猜测文件目标。
///
/// @author kongweiguang
#[test]
fn file_target_requires_persisted_ssh_host_identity() {
    let host_id = HostId::new();
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: None,
        command: None,
    });
    assert!(ssh_files_endpoint(&target, None, None, None, None).is_none());
    assert!(
        ssh_files_endpoint(
            &target,
            Some(&WorkspaceTargetRef::Local { profile_id: None }),
            None,
            None,
            None,
        )
        .is_none()
    );
}
