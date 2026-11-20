//! Root ToolRail 与中心共享 Entity 的宿主去重规则测试。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyView, App, AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, Window,
    base::TestSupportExt as _, div, prelude::*, px, size,
};
use kerminal_application::{
    AppError, AppResult,
    plugins::{
        PluginDescriptor, PluginId, PluginRegistry, PluginRegistryBuilder, ToolDescriptor, ToolId,
    },
    preferences::{SettingsSnapshot, ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId},
    sessions::{
        TerminalCommandAvailability, TerminalCommandContext, TerminalCommandUnavailableReason,
        UiSessionBinding,
    },
    workspace::{
        WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    connections::RemoteShellDialect,
    files::FileEndpoint,
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionOwner, SessionTarget, SshSessionTarget, TerminalSize},
    workspace::{SplitDirection, TerminalLayoutNode},
};

use super::super::CenterPanel;
use super::{
    center_preserves_terminal_file_context, center_tool_panel_close_is_current,
    center_tool_panel_escape_target, center_tool_panel_focus_on_open,
    center_tool_placement_focus_is_current, commands_content_enabled,
    compact_drawer_close_is_current, compact_drawer_escape_is_current, contains_tool,
    disabled_tool_rail_tools, docked_tool_panel_escape_target, expanded_center_tool_panel,
    local_files_endpoint, ssh_files_endpoint, tool_rail_panel_is_visible, tool_rail_tool_enabled,
    ui_binding_matches_pane,
};
use crate::{
    features::{
        workspace_controller::WorkspaceController, workspace_panes::WorkspacePaneEvent,
        workspace_tabs::WorkspaceTabEvent,
    },
    plugins::tools::{
        ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPresentation, ToolScope,
    },
    root::{
        keybinding_focus::classify_focus_scope,
        keybindings::KeybindingFocusScope,
        workbench::{
            ToolRailPanelSlots, ToolRailPanelState, compact_tool_panel_open_target,
            compact_tool_rail_panel_should_close,
        },
        workbench_layout::{WorkbenchLayoutInput, resolve_workbench_layout},
        workspace_render::WorkspaceRenderEvent,
        workspace_runtime::WorkspaceRuntime,
    },
};

// 共享命令服务 fixture 在 crate 根只声明一次，这里用 use 引入，避免重复载入。
use crate::features::agents::test_support::{FixtureAgentService, fixture_agent_view};
use crate::root_commands_service;

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

/// 四种 rail 宿主都属于工具面板；片段不能在底部或浮窗中切成中央命令工作区布局。
///
/// @author kongweiguang
#[test]
fn rail_tool_compact_projection_recognizes_every_panel_slot() {
    let tool = ToolRailToolId::Snippets;
    for panels in [
        ToolRailPanelSlots {
            attached: Some(tool),
            ..ToolRailPanelSlots::default()
        },
        ToolRailPanelSlots {
            left: Some(tool),
            ..ToolRailPanelSlots::default()
        },
        ToolRailPanelSlots {
            bottom: Some(tool),
            ..ToolRailPanelSlots::default()
        },
        ToolRailPanelSlots {
            center: Some(tool),
            ..ToolRailPanelSlots::default()
        },
    ] {
        assert!(contains_tool(panels, tool));
    }
    assert!(!contains_tool(ToolRailPanelSlots::default(), tool));
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
            left_tool_panel_preferred_width: 324.0,
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

/// 900px 断点以下首次打开工具时立即投影到紧凑抽屉，不等待第二次图标点击。
///
/// @author kongweiguang
#[test]
fn first_agent_open_in_compact_width_projects_its_requested_owner() {
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
        left_tool_panel_preferred_width: 324.0,
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
    assert!(before.compact_panels);
    assert!(before.effective_host_sidebar_collapsed);

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

/// main 仅按窗口宽度进入紧凑抽屉；短窗口的底部面板保持停靠，已关闭工具不能重挂宿主。
///
/// @author kongweiguang
#[test]
fn first_tool_open_uses_current_placement_geometry_and_never_reopens_a_closed_owner() {
    let tool = ToolRailToolId::AgentLauncher;
    let tools = [tool];
    for (placement, width, height, expected_compact) in [
        (ToolRailPanelPlacement::Attached, 1800.0, 833.0, false),
        (ToolRailPanelPlacement::Center, 832.0, 833.0, true),
        (ToolRailPanelPlacement::Left, 832.0, 833.0, true),
        (ToolRailPanelPlacement::Bottom, 1800.0, 400.0, false),
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
            left_tool_panel_preferred_width: 324.0,
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
        left_tool_panel_preferred_width: 324.0,
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

/// Compact 首次打开新工具只保留目标面板；已打开工具切换只改活动顺序，再点当前项只收起抽屉。
///
/// @author kongweiguang
#[test]
fn compact_new_tool_replaces_other_open_placements_without_changing_activation_semantics() {
    let previous = ToolRailToolId::AgentLauncher;
    let other = ToolRailToolId::Logs;
    let target = ToolRailToolId::Containers;
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(previous, ToolRailPanelPlacement::Attached);
    settings
        .panel_placements
        .insert(other, ToolRailPanelPlacement::Center);
    settings
        .panel_placements
        .insert(target, ToolRailPanelPlacement::Left);
    let mut existing = ToolRailPanelState::default();
    existing.toggle(&settings, previous);
    existing.toggle(&settings, other);
    assert_eq!(existing.project(&settings).panels.attached, Some(previous));
    assert_eq!(existing.project(&settings).panels.center, Some(other));

    let mut compact = existing.clone();
    let mut conflict_settings = settings.clone();
    conflict_settings
        .panel_placements
        .insert(other, ToolRailPanelPlacement::Attached);
    loop {
        let panels = compact.project(&conflict_settings).panels;
        let open_tool = [panels.attached, panels.left, panels.bottom, panels.center]
            .into_iter()
            .flatten()
            .next();
        let Some(open_tool) = open_tool else {
            break;
        };
        compact.toggle(&conflict_settings, open_tool);
    }
    compact.toggle(&conflict_settings, target);
    assert_eq!(
        compact.project(&conflict_settings).panels,
        ToolRailPanelSlots {
            left: Some(target),
            ..ToolRailPanelSlots::default()
        }
    );

    let mut activated_previous = existing.clone();
    activated_previous.activate(previous);
    assert_eq!(
        activated_previous
            .project(&conflict_settings)
            .panels
            .attached,
        Some(previous)
    );
    assert!(!compact_tool_rail_panel_should_close(
        true,
        Some(other),
        previous
    ));
    let mut activated_other = existing.clone();
    activated_other.activate(other);
    assert_eq!(
        activated_other.project(&conflict_settings).panels.attached,
        Some(other)
    );
    assert!(compact_tool_rail_panel_should_close(
        true,
        Some(other),
        other
    ));
    assert_eq!(
        activated_other.project(&conflict_settings).panels.attached,
        Some(other)
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
        left_tool_panel_preferred_width: 324.0,
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
            left_tool_panel_preferred_width: 324.0,
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

/// Docked Escape 只接受实际停靠的当前 owner；compact、折叠右 Dock、菜单/模态和旧回调保持未处理。
///
/// @author kongweiguang
#[test]
fn docked_escape_revalidates_slot_owner_visibility_and_surface() {
    let tool = ToolRailToolId::Snippets;
    let replacement = ToolRailToolId::Logs;
    for (placement, panels) in [
        (
            ToolRailPanelPlacement::Attached,
            ToolRailPanelSlots {
                attached: Some(tool),
                ..ToolRailPanelSlots::default()
            },
        ),
        (
            ToolRailPanelPlacement::Left,
            ToolRailPanelSlots {
                left: Some(tool),
                ..ToolRailPanelSlots::default()
            },
        ),
        (
            ToolRailPanelPlacement::Bottom,
            ToolRailPanelSlots {
                bottom: Some(tool),
                ..ToolRailPanelSlots::default()
            },
        ),
    ] {
        assert_eq!(
            docked_tool_panel_escape_target("Escape", tool, placement, panels, false, false, false),
            Some(tool)
        );
        assert_eq!(
            docked_tool_panel_escape_target("enter", tool, placement, panels, false, false, false),
            None
        );
        assert_eq!(
            docked_tool_panel_escape_target(
                "Escape",
                replacement,
                placement,
                panels,
                false,
                false,
                false
            ),
            None
        );
        assert_eq!(
            docked_tool_panel_escape_target("Escape", tool, placement, panels, true, false, false),
            None
        );
        assert_eq!(
            docked_tool_panel_escape_target("Escape", tool, placement, panels, false, false, true),
            None
        );
    }

    let attached_panels = ToolRailPanelSlots {
        attached: Some(tool),
        ..ToolRailPanelSlots::default()
    };
    assert_eq!(
        docked_tool_panel_escape_target(
            "Escape",
            tool,
            ToolRailPanelPlacement::Attached,
            attached_panels,
            false,
            true,
            false,
        ),
        None,
        "a stale right-dock callback cannot expand a panel that is already collapsed"
    );
    assert_eq!(
        docked_tool_panel_escape_target(
            "Escape",
            tool,
            ToolRailPanelPlacement::Center,
            ToolRailPanelSlots {
                center: Some(tool),
                ..ToolRailPanelSlots::default()
            },
            false,
            false,
            false,
        ),
        None,
        "center keeps its separate focus-owner path"
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
                left_tool_panel_preferred_width: 324.0,
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

/// 投影票据必须区分代次而不是只比较值：A→B→A 后旧 A effect 不能重新命中，
/// 同一代只发起一次插件生命周期，显式 invalidate 后才允许同值再次尝试。
///
/// @author kongweiguang
#[test]
fn projection_ticket_rejects_stale_aba_and_deduplicates_lifecycle_attempts() {
    use crate::plugins::tools::{PluginTools, ToolContext, ToolProjection};

    let mut tools = PluginTools::default();
    let projection_a = ToolProjection {
        context: ToolContext::default(),
        visible: vec![ToolRailToolId::System],
        window_active: true,
    };
    let ticket_a = tools.stage_projection(projection_a.clone());
    assert!(tools.projection_is_current(ticket_a));
    assert!(tools.projection_lifecycle_needs_attempt(ticket_a));

    let mut projection_b = projection_a.clone();
    projection_b.visible = vec![ToolRailToolId::Ports];
    let ticket_b = tools.stage_projection(projection_b);
    assert!(!tools.projection_is_current(ticket_a));
    assert!(tools.projection_is_current(ticket_b));
    assert_ne!(ticket_a.generation(), ticket_b.generation());

    // 投影值在 ABA 流程中多次重放且 ToolProjection 未实现 Copy：非末次使用一律克隆，
    // 只有最后一次 stage_projection 可以直接消费所有权。
    let ticket_a_again = tools.stage_projection(projection_a.clone());
    assert!(!tools.projection_is_current(ticket_a));
    assert!(tools.projection_is_current(ticket_a_again));
    assert!(ticket_a_again.generation() > ticket_a.generation());

    tools.mark_projection_lifecycle_attempted(ticket_a_again);
    assert!(!tools.projection_lifecycle_needs_attempt(ticket_a_again));
    assert_eq!(
        tools.attempted_projection_generations_for_test(),
        vec![ticket_a_again.generation()]
    );

    tools.invalidate_projection();
    // ABA 重试要求重放与首次完全相同的投影值；此处是最后使用，直接消费所有权即可。
    let ticket_retry = tools.stage_projection(projection_a);
    assert!(tools.projection_is_current(ticket_retry));
    assert!(tools.projection_lifecycle_needs_attempt(ticket_retry));
}

/// 窗口失焦或工具未挂载时，实际插件投影不得将工具标记为活动。
///
/// @author kongweiguang
#[test]
fn activity_requires_window_focus_and_a_visible_host() {
    use crate::plugins::tools::{PluginTools, ToolContext, ToolProjection};

    let mut tools = PluginTools::default();
    let mut projection = ToolProjection {
        context: ToolContext::default(),
        visible: vec![ToolRailToolId::System],
        window_active: false,
    };
    tools.stage_projection(projection.clone());
    assert!(!tools.projected_tool_active(ToolRailToolId::System));

    projection.window_active = true;
    tools.stage_projection(projection);
    assert!(tools.projected_tool_active(ToolRailToolId::System));
    assert!(!tools.projected_tool_active(ToolRailToolId::Ports));
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

/// 构造带内置与外部贡献的冻结目录，验证门禁只读取启用视图而不改写声明。
///
/// @author kongweiguang
fn gating_registry(builtin_states: &[(&str, bool)]) -> PluginRegistry {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    // 同名外部 provider 分别覆盖启用与禁用来源，确认 Plugin 工具按 owner 状态过滤。
    for (provider, enabled) in [("acme.panel", true), ("acme.off", false)] {
        let id = PluginId::new(provider).expect("valid provider");
        builder.register_provider(PluginDescriptor::new(id.clone(), provider, enabled));
        builder.register_tool(ToolDescriptor::new(
            ToolId::new(format!("{provider}/panel")).expect("valid tool"),
            id,
            "Panel",
            None,
        ));
    }
    let states = builtin_states
        .iter()
        .map(|(id, enabled)| (PluginId::new(*id).expect("valid unit id"), *enabled))
        .collect::<BTreeMap<_, _>>();
    builder
        .freeze_with_builtin_states(&states)
        .expect("valid registry")
}

/// 右栏集中门禁同时覆盖内置与外部工具：禁用只影响对应 owner，
/// 其余内置工具和启用中的外部贡献继续可用。
///
/// @author kongweiguang
#[test]
fn rail_gate_reads_frozen_builtin_and_external_enablement() {
    let registry = gating_registry(&[("kerminal.sftp", false), ("kerminal.tmux", false)]);
    assert!(!tool_rail_tool_enabled(&registry, ToolRailToolId::Sftp));
    assert!(!tool_rail_tool_enabled(&registry, ToolRailToolId::Tmux));
    assert!(tool_rail_tool_enabled(&registry, ToolRailToolId::Snippets));
    assert!(tool_rail_tool_enabled(&registry, ToolRailToolId::Logs));

    let enabled_plugin =
        ToolRailToolId::from_id("acme.panel/panel").expect("valid namespaced tool id");
    let disabled_plugin =
        ToolRailToolId::from_id("acme.off/panel").expect("valid namespaced tool id");
    assert!(tool_rail_tool_enabled(&registry, enabled_plugin));
    assert!(!tool_rail_tool_enabled(&registry, disabled_plugin));
}

/// 空目录是独立界面/测试的未装配模式；不因缺少插件服务而清空全部 Rail 入口。
///
/// @author kongweiguang
#[test]
fn empty_registry_keeps_standalone_rail_entries_available() {
    let registry = PluginRegistry::default();
    assert!(tool_rail_tool_enabled(&registry, ToolRailToolId::Sftp));
    assert!(tool_rail_tool_enabled(&registry, ToolRailToolId::Logs));
    assert!(commands_content_enabled(&registry));
    assert!(disabled_tool_rail_tools(&registry).is_empty());
}

/// SSH 是 tmux、服务器信息、容器与端口转发的真实执行通道；关闭 SSH 后这些入口
/// 必须与 MCP guides_tools 的 ssh-command/tmux/server-info/container/port-forward
/// 规则一致地消失，而只依赖自身单元的 SFTP、日志、片段和 Agent 不受影响。
///
/// @author kongweiguang
#[test]
fn ssh_dependency_hides_operations_tools_only() {
    let registry = gating_registry(&[("kerminal.ssh", false)]);
    for tool in [
        ToolRailToolId::Tmux,
        ToolRailToolId::System,
        ToolRailToolId::Containers,
        ToolRailToolId::Ports,
    ] {
        assert!(
            !tool_rail_tool_enabled(&registry, tool),
            "{tool:?} must require the SSH unit"
        );
    }
    for tool in [
        ToolRailToolId::Sftp,
        ToolRailToolId::Logs,
        ToolRailToolId::Snippets,
        ToolRailToolId::AgentLauncher,
        ToolRailToolId::Context,
    ] {
        assert!(
            tool_rail_tool_enabled(&registry, tool),
            "{tool:?} keeps only its own unit"
        );
    }
    assert_eq!(
        disabled_tool_rail_tools(&registry),
        BTreeSet::from([
            ToolRailToolId::Containers,
            ToolRailToolId::Ports,
            ToolRailToolId::System,
            ToolRailToolId::Tmux,
            ToolRailToolId::from_id("acme.off/panel").expect("valid disabled plugin id"),
        ]),
        "command palette candidates must skip the same SSH-dependent and disabled-plugin tools"
    );

    // 恢复 SSH 后同一持久偏好重新投影，确认过滤只读取启用视图而不删除偏好。
    let restored = gating_registry(&[]);
    assert!(tool_rail_tool_enabled(&restored, ToolRailToolId::Tmux));
    assert!(tool_rail_tool_enabled(&restored, ToolRailToolId::Ports));
    let restored_disabled = disabled_tool_rail_tools(&restored);
    for tool in [
        ToolRailToolId::Tmux,
        ToolRailToolId::System,
        ToolRailToolId::Containers,
        ToolRailToolId::Ports,
    ] {
        assert!(
            !restored_disabled.contains(&tool),
            "{tool:?} returns after SSH is re-enabled"
        );
    }
}

/// ports 与 containers 的自身单元仍由 owner 判定；关闭自身单元不能只靠 SSH 恢复。
///
/// @author kongweiguang
#[test]
fn operations_owner_unit_still_required_when_ssh_enabled() {
    let registry = gating_registry(&[("kerminal.ports", false), ("kerminal.containers", false)]);
    assert!(!tool_rail_tool_enabled(&registry, ToolRailToolId::Ports));
    assert!(!tool_rail_tool_enabled(
        &registry,
        ToolRailToolId::Containers
    ));
    assert!(tool_rail_tool_enabled(&registry, ToolRailToolId::Tmux));
    assert!(tool_rail_tool_enabled(&registry, ToolRailToolId::System));
}

/// 片段内容由 commands 单元承载；程序化 OpenSnippetPanel 入口和 Rail 图标
/// 必须共享同一判定，禁用时保持原工作区而不切换到命令中心页。
///
/// @author kongweiguang
#[test]
fn commands_unit_gates_snippet_content_entry() {
    let registry = gating_registry(&[("kerminal.commands", false)]);
    assert!(!commands_content_enabled(&registry));
    assert!(!tool_rail_tool_enabled(&registry, ToolRailToolId::Snippets));
    assert!(disabled_tool_rail_tools(&registry).contains(&ToolRailToolId::Snippets));
    assert!(tool_rail_tool_enabled(
        &gating_registry(&[]),
        ToolRailToolId::Snippets
    ));
    assert!(commands_content_enabled(&gating_registry(&[])));
}

/// 在真实工作区画布分发键盘事件，验证按下、长按、松开都遵守同一会话的粘贴归属。
///
/// @author kongweiguang
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[gpui_kit::test]
fn standard_paste_gestures_dispatch_once_to_the_focused_terminal(app: &mut TestAppContext) {
    use std::{cell::RefCell, rc::Rc};

    use gpui_kit::{ClipboardItem, KeyDownEvent, KeyUpEvent, Keystroke};
    use kerminal_terminal::{
        TerminalEngine, TerminalLimits,
        input::{KeyPhase, TerminalInputEvent, TerminalKey},
        snapshot::RenderSnapshotRequest,
    };

    use crate::root::pane_input::PaneTerminalInputEvent;

    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    visual.simulate_resize(size(px(1020.), px(720.)));
    settle_split_fixture(&runtime, visual);

    let tab = TabId::new();
    let pane = PaneId::new();
    let session = SessionId::new();
    let binding = split_binding(session.clone(), tab.clone(), pane.clone(), 1);
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    let payload = "剪贴板\nsecond line";
    let _subscription = visual.update(|window, app| {
        window.activate_window();
        app.write_to_clipboard(ClipboardItem::new_string(payload.to_owned()));
        let input = root.update(app, |root, cx| {
            let mut controller = WorkspaceController::from_missing().unwrap();
            controller
                .synchronize_snapshot(WorkspaceSessionSnapshot {
                    active_tab_id: Some(tab.clone()),
                    focused_pane_id: Some(pane.clone()),
                    selected_machine_id: Some("local".to_owned()),
                    terminal_tabs: vec![WorkspaceSessionTab::Terminal {
                        id: tab.clone(),
                        tab_group_id: None,
                        title: "shell".to_owned(),
                        machine_id: "local".to_owned(),
                        layout: TerminalLayoutNode::pane(pane.clone()),
                    }],
                    terminal_panes: vec![split_workspace_pane(pane.clone())],
                    pinned_containers: Vec::new(),
                    tab_groups: BTreeMap::new(),
                })
                .unwrap();
            root.workspace_runtime =
                WorkspaceRuntime::from_snapshot(controller.snapshot()).unwrap();
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            let size = TerminalSize::default();
            root.workspace_runtime
                .install_session(&pane, session.clone(), split_session_target(), size)
                .unwrap();
            let mut engine =
                TerminalEngine::new(session.clone(), size, TerminalLimits::default()).unwrap();
            let mut snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
            // 全键与事件类型一起开启，才能发现粘贴键泄漏的 Insert 或普通字母 v 释放。
            snapshot.modes.kitty_keyboard_flags = 0b01011;
            snapshot.modes.bracketed_paste = true;
            assert!(
                root.workspace_runtime
                    .accept_initial_snapshot(&pane, &session, snapshot)
                    .unwrap()
            );
            root.install_pane_input_handler(
                binding.clone(),
                split_command_context(&binding),
                window,
                cx,
            );
            let input = root.pane_input_handlers[&session].entity.clone();
            window.focus(&input.read(cx).focus_handle(), cx);
            root.refresh_workspace_views(cx);
            input
        });
        window.draw(app).clear(app);
        app.subscribe(&input, move |_, event: &PaneTerminalInputEvent, _| {
            if !matches!(event.event, TerminalInputEvent::Focus(_)) {
                observed
                    .borrow_mut()
                    .push((event.binding.clone(), event.event.clone()));
            }
        })
    });

    // 每种手势复用同一断言，避免条件编译让 Linux 只剩单元素循环。
    let mut assert_paste_gesture = |gesture: &str, main_key: &str| {
        events.borrow_mut().clear();
        let released_modifiers = Keystroke::parse(main_key).unwrap();
        visual.simulate_event(KeyDownEvent {
            keystroke: Keystroke::parse(gesture).unwrap(),
            is_held: false,
            prefer_character_input: false,
        });
        visual.simulate_event(KeyDownEvent {
            keystroke: released_modifiers.clone(),
            is_held: true,
            prefer_character_input: false,
        });
        visual.simulate_event(KeyUpEvent {
            keystroke: released_modifiers,
        });
        assert_eq!(
            events.borrow().as_slice(),
            &[(
                binding.clone(),
                TerminalInputEvent::Paste(payload.to_owned())
            )],
            "{gesture}"
        );
    };
    assert_paste_gesture("shift-insert", "insert");
    #[cfg(target_os = "windows")]
    assert_paste_gesture("ctrl-shift-v", "v");

    visual.simulate_event(KeyDownEvent {
        keystroke: Keystroke::parse("insert").unwrap(),
        is_held: false,
        prefer_character_input: false,
    });
    assert!(matches!(
        events.borrow().as_slice(),
        [_, (target, TerminalInputEvent::Key(event))]
            if target == &binding && event.key == TerminalKey::Insert && event.phase == KeyPhase::Press
    ));

    // 直接调用只读路由，检查 Ctrl 先松时长按 C 不会把复制反馈换成重连提示。
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let copy = KeyDownEvent {
                keystroke: Keystroke::parse("ctrl-shift-c").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            assert!(root.handle_terminal_history_key_down(binding.clone(), &copy, window, cx));
            let copy_status = root.status.clone();
            let held = KeyDownEvent {
                keystroke: Keystroke::parse("c").unwrap(),
                is_held: true,
                prefer_character_input: false,
            };
            assert!(root.handle_terminal_history_key_down(binding.clone(), &held, window, cx));
            assert_eq!(root.status, copy_status);
        });
    });
}

/// 经真实 Root 的 `handle_workspace_render_event` 激活分屏 pane，且不预置投影或改动
/// effect 调度；事件返回后、deferred effect 排空前手动绘制恰好一帧，检查首个元素树中的
/// Logs 目标、保留插件实体和渲染上下文。成功路径要求 B 已提交、A 不可挂载、实体不重建，
/// 失败路径注入 A 的 `context_changed` 失败并要求旧 B 与新 A 均不可挂载、显示局部重试面。
/// 隐藏失败后还会重复真实 render 入口，要求同一代 apply 不重发显隐回调，只保留显式重试。
/// 它只证明无头 App update 内的首绘树，不证明真实窗口 present、合成或视觉闪烁。
///
/// @author kongweiguang
#[gpui_kit::test]
fn split_focus_switch_keeps_retained_plugin_tool_mounted(app: &mut TestAppContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| {
            let (commands, _) =
                root_commands_service::empty_commands_service_with_calls(services.sessions.clone());
            services.with_commands(Arc::new(commands))
        });
    visual.simulate_resize(size(px(1440.), px(900.)));
    settle_split_fixture(&runtime, visual);

    let tab_id = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let session_a = SessionId::new();
    let session_b = SessionId::new();
    // 会话标识之后还要用于给两个 pane 安装 runtime 会话，绑定构造只借用一份克隆。
    let binding_a = split_binding(session_a.clone(), tab_id.clone(), pane_a.clone(), 1);
    let binding_b = split_binding(session_b.clone(), tab_id.clone(), pane_b.clone(), 2);

    let tool = ToolRailToolId::from_id(SPLIT_TOOL_ID).expect("valid fixture tool id");
    let provider = PluginId::new("fixture.split-provider").expect("valid provider");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    builder.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    builder.register_tool(ToolDescriptor::new(
        ToolId::new(tool.as_str()).expect("valid fixture tool id"),
        provider,
        "Split fixture",
        None,
    ));
    let registry = builder.freeze().expect("valid registry");
    let state = Arc::new(SplitFixtureState::default());
    let created = Arc::new(Mutex::new(None));
    let factory = Arc::new(SplitFixtureFactory {
        state: state.clone(),
        created: created.clone(),
        tool_id: SPLIT_TOOL_ID,
    });

    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            let mut controller = WorkspaceController::from_missing().expect("empty workspace");
            let layout = TerminalLayoutNode::pane(pane_a.clone())
                .split_pane(
                    &pane_a,
                    pane_b.clone(),
                    SplitDirection::Vertical,
                    "split-a-b",
                    false,
                )
                .expect("valid split layout");
            controller
                .synchronize_snapshot(WorkspaceSessionSnapshot {
                    pinned_containers: Vec::new(),
                    active_tab_id: Some(tab_id.clone()),
                    focused_pane_id: Some(pane_a.clone()),
                    selected_machine_id: Some("local".to_owned()),
                    terminal_tabs: vec![WorkspaceSessionTab::Terminal {
                        id: tab_id.clone(),
                        tab_group_id: None,
                        title: "split".to_owned(),
                        machine_id: "local".to_owned(),
                        layout,
                    }],
                    terminal_panes: vec![
                        split_workspace_pane(pane_a.clone()),
                        split_workspace_pane(pane_b.clone()),
                    ],
                    tab_groups: BTreeMap::new(),
                })
                .expect("valid split workspace");
            root.workspace_runtime =
                WorkspaceRuntime::from_snapshot(controller.snapshot()).expect("valid runtime");
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            for (pane, session) in [(&pane_a, session_a.clone()), (&pane_b, session_b.clone())] {
                root.workspace_runtime
                    .install_session(
                        pane,
                        session,
                        split_session_target(),
                        TerminalSize::default(),
                    )
                    .expect("fixture session");
            }
            root.install_pane_input_handler(
                binding_a.clone(),
                split_command_context(&binding_a),
                window,
                cx,
            );
            root.install_pane_input_handler(
                binding_b.clone(),
                split_command_context(&binding_b),
                window,
                cx,
            );
            root.plugin_tools
                .register(&registry, factory.clone())
                .expect("fixture factory");
            let mut tool_rail = ToolRailSettings::default();
            tool_rail
                .panel_placements
                .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Left);
            root.settings_snapshot = Some(SettingsSnapshot {
                tool_rail,
                ..Default::default()
            });
            root.toggle_tool_rail_panel(ToolRailToolId::Logs, window, cx);
            root.toggle_tool_rail_panel(tool, window, cx);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    for _ in 0..4 {
        visual.update(|window, app| window.draw(app).clear(app));
        settle_split_fixture(&runtime, visual);
    }

    let fixture_view = created
        .lock()
        .expect("fixture entity lock")
        .clone()
        .expect("factory must create the retained view");
    visual.update(|window, app| {
        let context = root.update(app, |root, cx| root.plugin_tool_context(cx));
        assert_eq!(
            context.active_terminal(),
            Some(&binding_a),
            "initial split focus must project pane A through the production logs target"
        );
        assert!(
            root.read_with(app, |root, _| root
                .plugin_tools
                .view_for_mount(tool, &context, window.is_window_active())
                .is_some()),
            "the opened fixture tool must be mountable before the split switch"
        );
    });

    visual.update(|window, app| {
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clear();
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        let rendered_context = state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .last()
            .cloned()
            .flatten()
            .expect("the fixture must render before the split switch");
        assert_eq!(rendered_context.active_terminal(), Some(&binding_a));
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "the opened fixture tool must already be mounted in the attached dock"
        );
    });

    let context_a =
        ToolContext::new(Some(tab_id.clone()), None).with_active_terminal(Some(binding_a.clone()));
    let context_b =
        ToolContext::new(Some(tab_id.clone()), None).with_active_terminal(Some(binding_b.clone()));
    let renders_before = state.renders.load(Ordering::SeqCst);
    // 真实 Root 路径：不预置投影、不改变 effect 调度。事件返回后、deferred effect 排空前
    // 手动绘制恰好一帧，直接检查首个元素树，而不是等 GPUI 收敛后再看结果。
    visual.update(|window, app| {
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clear();
        root.update(app, |root, cx| {
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Activate(pane_b.clone()),
                },
                window,
                cx,
            );
        });
        let logs_after_event =
            root.read_with(app, |root, _| root.projected_logs_target_for_test.clone());
        // 主动通知保留实体只制造一次真实 render，不参与投影或 effect 调度。
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        let logs_after_draw =
            root.read_with(app, |root, _| root.projected_logs_target_for_test.clone());
        let (mount_a, mount_b) = root.read_with(app, |root, _| {
            let window_active = window.is_window_active();
            (
                root.plugin_tools
                    .view_for_mount(tool, &context_a, window_active)
                    .is_some(),
                root.plugin_tools
                    .view_for_mount(tool, &context_b, window_active)
                    .is_some(),
            )
        });
        let first_draw_contexts = state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clone();
        let created_views = state
            .created_views
            .lock()
            .expect("created views lock")
            .clone();
        assert_eq!(
            logs_after_event
                .as_ref()
                .map(|target| target.binding().clone()),
            Some(binding_b.clone()),
            "the focus event must project pane B to the built-in Logs target before any draw"
        );
        assert_eq!(
            logs_after_draw
                .as_ref()
                .map(|target| target.binding().clone()),
            Some(binding_b.clone()),
            "the first draw must reuse the pane B Logs target without a stale A handoff"
        );
        assert!(
            mount_b && !mount_a,
            "the first draw may mount only the committed pane B context"
        );
        assert_eq!(
            created_views.len(),
            1,
            "the retained Tab tool entity must not be recreated by the focus handoff"
        );
        assert_eq!(
            created_views[0].entity_id(),
            fixture_view.entity_id(),
            "the mounted view must be the original retained entity"
        );
        assert!(
            !first_draw_contexts.is_empty()
                && first_draw_contexts.iter().all(|context| context
                    .as_ref()
                    .is_some_and(|context| context.active_terminal() == Some(&binding_b))),
            "the first draw must never render the old pane A target"
        );
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        let retry_id: gpui_kit::ElementId = "retry-plugin-tool-fixture.split-tool".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        assert!(
            snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "the first draw must mount the plugin content, not fallback"
        );
        assert!(
            !snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&retry_id)),
            "a successfully projected target must not show the local recovery surface"
        );
    });
    settle_split_fixture(&runtime, visual);
    assert!(
        state
            .committed_history
            .lock()
            .expect("context lock")
            .iter()
            .any(|context| context.active_terminal() == Some(&binding_b)),
        "the deferred projection must commit pane B to the retained tool instance"
    );
    visual.update(|window, app| {
        let context = root.update(app, |root, cx| root.plugin_tool_context(cx));
        assert_eq!(context.active_terminal(), Some(&binding_b));
        assert!(root.read_with(app, |root, _| {
            root.plugin_tools
                .view_for_mount(tool, &context, window.is_window_active())
                .is_some()
        }));
        assert!(root.read_with(app, |root, _| {
            root.plugin_tools
                .view_for_mount(tool, &context_a, window.is_window_active())
                .is_none()
        }));
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        let retry_id: gpui_kit::ElementId = "retry-plugin-tool-fixture.split-tool".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        assert!(
            snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "the retained tool content must be visible after the deferred apply"
        );
        assert!(
            !snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&retry_id)),
            "a successful context update must not leave the failure retry surface visible"
        );
    });
    assert!(
        state.renders.load(Ordering::SeqCst) > renders_before,
        "the retained view must render with the new context after the deferred apply"
    );
    assert!(
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .iter()
            .all(|context| {
                context
                    .as_ref()
                    .is_some_and(|context| context.active_terminal() == Some(&binding_b))
            }),
        "after the split switch the retained view must never render with pane A"
    );

    // 窗口失活再激活：两个方向的首个绘制帧都必须直接使用当前 window_active 的已确认
    // 工具投影，不能先画 unavailable 占位再 defer 恢复；同一保活实体和目标身份保持不变。
    let entity_before_reactivation = fixture_view.entity_id();
    let target_before_reactivation = visual.update(|_, app| {
        root.read_with(app, |root, _| root.projected_logs_target_for_test.clone())
    });
    visual.update(|_, app| {
        root.update(app, |root, _| {
            root.plugin_tools.reset_unavailable_fallback_renders()
        });
    });
    visual.deactivate_window();
    visual.update(|window, app| {
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clear();
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        assert!(
            !window.is_window_active(),
            "the deactivation first frame must project the inactive window"
        );
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        let retry_id: gpui_kit::ElementId = "retry-plugin-tool-fixture.split-tool".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        let live_visible = snapshots
            .iter()
            .any(|node| node.visible() && node.path().contains(&live_id));
        let retry_visible = snapshots
            .iter()
            .any(|node| node.visible() && node.path().contains(&retry_id));
        assert!(
            live_visible,
            "the first deactivation frame must mount the retained tool content"
        );
        assert!(
            !retry_visible,
            "deactivation must not flash the unavailable recovery surface"
        );
    });
    settle_split_fixture(&runtime, visual);
    visual.update(|window, _| window.activate_window());
    // 只绘制激活后的第一帧，不先跑 deferred effect；修复必须在事件阶段已提交 (true, true)。
    visual.run_until_parked();
    visual.update(|window, app| {
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clear();
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        assert!(
            window.is_window_active(),
            "the activation first frame must project the active window"
        );
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        let retry_id: gpui_kit::ElementId = "retry-plugin-tool-fixture.split-tool".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        assert!(
            snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "the first draw after reactivation must mount the retained tool content"
        );
        assert!(
            !snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&retry_id)),
            "reactivation must not flash the unavailable recovery surface"
        );
        let created_views = state
            .created_views
            .lock()
            .expect("created views lock")
            .clone();
        assert_eq!(
            created_views.len(),
            1,
            "reactivation must not recreate the retained tool"
        );
        assert_eq!(
            created_views[0].entity_id(),
            entity_before_reactivation,
            "reactivation must keep the original retained tool entity"
        );
        assert_eq!(
            root.read_with(app, |root, _| root.projected_logs_target_for_test.clone()),
            target_before_reactivation,
            "reactivation must not rebind the tool target"
        );
    });
    visual.update(|_, app| {
        let fallback_renders = root
            .read(app)
            .plugin_tools
            .unavailable_fallback_render_count();
        assert_eq!(
            fallback_renders, 0,
            "the deactivation/activation transition must never render the unavailable fallback"
        );
    });

    // 失败路径：切回 A 并让 A 的 context_changed 失败。此时若宿主放宽门禁，
    // 用户会继续操作旧 B 目标的工具；回归要求旧目标立即不可挂载，并展示真实的局部恢复面。
    state
        .fail_terminals
        .lock()
        .expect("failure lock")
        .push(binding_a.clone());
    let renders_before_failure = state.renders.load(Ordering::SeqCst);
    // 真实 Root 路径：失败注入后在 deferred effect 排空前绘制恰好一帧，检查失败门禁
    // 是否已经阻止旧 B 视图进入首棵树；不预置投影或改变 effect 调度。
    visual.update(|window, app| {
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clear();
        root.update(app, |root, cx| {
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Activate(pane_a.clone()),
                },
                window,
                cx,
            );
        });
        let logs_after_event =
            root.read_with(app, |root, _| root.projected_logs_target_for_test.clone());
        // 失败视图即使被通知也不能重新渲染旧目标；这里只制造一次潜在 render。
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        let logs_after_draw =
            root.read_with(app, |root, _| root.projected_logs_target_for_test.clone());
        let window_active = window.is_window_active();
        let mount_a = root.read_with(app, |root, _| {
            root.plugin_tools
                .view_for_mount(tool, &context_a, window_active)
                .is_some()
        });
        let mount_b = root.read_with(app, |root, _| {
            root.plugin_tools
                .view_for_mount(tool, &context_b, window_active)
                .is_some()
        });
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        let retry_id: gpui_kit::ElementId = "retry-plugin-tool-fixture.split-tool".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        let live_visible = snapshots
            .iter()
            .any(|node| node.visible() && node.path().contains(&live_id));
        let retry_visible = snapshots
            .iter()
            .any(|node| node.visible() && node.path().contains(&retry_id));
        assert_eq!(
            logs_after_event
                .as_ref()
                .map(|target| target.binding().clone()),
            Some(binding_a.clone()),
            "the focus event must project pane A to Logs even when the plugin rejects it"
        );
        assert_eq!(
            logs_after_draw
                .as_ref()
                .map(|target| target.binding().clone()),
            Some(binding_a.clone()),
            "the failed first draw must not roll the Logs target back to pane B"
        );
        assert!(
            !mount_b && !mount_a,
            "the first draw must reject both the stale B and newly failed A plugin contexts"
        );
        assert!(
            !live_visible && retry_visible,
            "the first draw must show the local recovery surface instead of stale content"
        );
        assert!(
            state
                .rendered_contexts
                .lock()
                .expect("render context lock")
                .is_empty(),
            "a failed context update must not render a stale or rejected target"
        );
    });
    settle_split_fixture(&runtime, visual);
    let (committed_context, mount_a, mount_b, live_visible, retry_visible) =
        visual.update(|window, app| {
            let window_active = window.is_window_active();
            let committed_context = state
                .committed_context
                .lock()
                .expect("committed context lock")
                .clone();
            let mount_a = root.read_with(app, |root, _| {
                root.plugin_tools
                    .view_for_mount(tool, &context_a, window_active)
                    .is_some()
            });
            let mount_b = root.read_with(app, |root, _| {
                root.plugin_tools
                    .view_for_mount(tool, &context_b, window_active)
                    .is_some()
            });
            let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
            let retry_id: gpui_kit::ElementId = "retry-plugin-tool-fixture.split-tool".into();
            let snapshots = gpui_kit::base::test_support::snapshots(window);
            let live_visible = snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id));
            let retry_visible = snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&retry_id));
            (
                committed_context,
                mount_a,
                mount_b,
                live_visible,
                retry_visible,
            )
        });
    assert_eq!(
        state.renders.load(Ordering::SeqCst),
        renders_before_failure,
        "a failed context update must unmount the stale target instead of rendering old pane B"
    );
    assert!(
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .iter()
            .all(|context| context.is_none()),
        "a failed context update must never render the stale or newly rejected target"
    );
    assert!(
        !mount_b,
        "old pane B must not stay mountable after the failed update"
    );
    assert!(!mount_a, "the rejected pane A target must not be mountable");
    assert!(
        !live_visible && retry_visible,
        "a rejected context update must surface the local retry state, not the stale tool: \
         committed={committed_context:?}, live={live_visible}, retry={retry_visible}, mount_A={mount_a}, mount_B={mount_b}"
    );
    assert!(
        !state
            .failed_contexts
            .lock()
            .expect("failure history lock")
            .is_empty(),
        "the failure phase must exercise context_changed rather than only the mount gate"
    );

    // 同代重复焦点：值相同的投影不能再调用失败的 context_changed，也不能追加生命周期代次。
    let failed_before_repeat = state
        .failed_contexts
        .lock()
        .expect("failure history lock")
        .len();
    let context_attempts_before_repeat = state
        .context_attempts
        .lock()
        .expect("context attempt lock")
        .len();
    let generations_before_repeat = visual.update(|_, app| {
        root.read_with(app, |root, _| {
            root.plugin_tools
                .attempted_projection_generations_for_test()
        })
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Activate(pane_a.clone()),
                },
                window,
                cx,
            );
        });
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
    });
    let (
        failed_after_repeat,
        context_attempts_after_repeat,
        generations_after_repeat,
        mount_after_repeat,
    ) = visual.update(|window, app| {
        let mount = root.read_with(app, |root, _| {
            root.plugin_tools
                .view_for_mount(tool, &context_a, window.is_window_active())
                .is_some()
        });
        (
            state
                .failed_contexts
                .lock()
                .expect("failure history lock")
                .len(),
            state
                .context_attempts
                .lock()
                .expect("context attempt lock")
                .len(),
            root.read_with(app, |root, _| {
                root.plugin_tools
                    .attempted_projection_generations_for_test()
            }),
            mount,
        )
    });
    assert!(
        !mount_after_repeat,
        "a failed context must stay unmountable on repeat focus"
    );
    assert_eq!(
        failed_after_repeat, failed_before_repeat,
        "the same projection generation must not retry a failed context_changed callback"
    );
    assert_eq!(
        context_attempts_after_repeat, context_attempts_before_repeat,
        "repeat focus must not reach the plugin context callback again"
    );
    assert_eq!(
        generations_after_repeat, generations_before_repeat,
        "repeat focus must not mark a new lifecycle attempt for the same projection"
    );

    // 显式重试：清除注入失败后 invalidate 并立刻提交事件投影，首个绘制帧必须恢复原实体。
    state.fail_terminals.lock().expect("failure lock").clear();
    let context_attempts_before_retry = state
        .context_attempts
        .lock()
        .expect("context attempt lock")
        .len();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(
                root.prepare_plugin_tool(tool, window, cx),
                "explicit retry must reuse the retained Open instance"
            );
            root.sync_tool_projection_before_first_draw(window, cx);
        });
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        let committed_context = state
            .committed_context
            .lock()
            .expect("committed context lock")
            .clone();
        assert_eq!(
            committed_context
                .as_ref()
                .and_then(|context| context.active_terminal()),
            Some(&binding_a),
            "explicit retry must commit the current pane A target"
        );
        assert!(
            root.read_with(app, |root, _| root
                .plugin_tools
                .view_for_mount(tool, &context_a, window.is_window_active())
                .is_some()),
            "explicit retry must restore the retained tool mount"
        );
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        let retry_id: gpui_kit::ElementId = "retry-plugin-tool-fixture.split-tool".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        assert!(
            snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "the retry first draw must mount the real plugin content"
        );
        assert!(
            !snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&retry_id)),
            "successful retry must remove the local recovery surface before first draw"
        );
    });
    assert_eq!(
        state
            .context_attempts
            .lock()
            .expect("context attempt lock")
            .len(),
        context_attempts_before_retry + 1,
        "explicit retry must invoke context_changed exactly once on the new generation"
    );

    // 隐藏路径：render 先暂存隐藏投影但不执行回调；事件阶段即使 stage 相同值也必须应用，
    // 一次 visibility_changed(false) 失败后不得把旧可见状态写成已隐藏，也不得同代重试。
    state
        .fail_visibility_once
        .lock()
        .expect("visibility failure lock")
        .push(binding_a.clone());
    let visibility_before_hide = state
        .visibility_attempts
        .lock()
        .expect("visibility attempt lock")
        .len();
    visual.update(|window, app| {
        let window_active = window.is_window_active();
        root.update(app, |root, cx| {
            root.right_dock_collapsed = true;
            let _ = root.workbench_tool_panels(window, cx);
            assert_eq!(
                state
                    .visibility_attempts
                    .lock()
                    .expect("visibility attempt lock")
                    .len(),
                visibility_before_hide,
                "render staging must not execute plugin visibility callbacks"
            );
            assert!(
                root.plugin_tools
                    .view_for_mount(tool, &context_a, window_active)
                    .is_some(),
                "a staged but unapplied hide must keep the last confirmed mount"
            );
            root.sync_tool_projection_before_first_draw(window, cx);
        });
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        let attempts = state
            .visibility_attempts
            .lock()
            .expect("visibility attempt lock")
            .clone();
        let status = root.read_with(app, |root, _| root.status.as_ref().map(ToString::to_string));
        assert_eq!(
            attempts.len(),
            visibility_before_hide + 1,
            "the event-phase apply must attempt the hide despite the render stage matching"
        );
        assert_eq!(
            attempts.last(),
            Some(&(binding_a.clone(), false, window_active)),
            "the hide callback must target the actually confirmed pane A"
        );
        assert!(status.as_deref().is_some_and(|status| {
            status.contains("未确认") && status.contains("重新打开")
        }));
        assert!(
            root.read_with(app, |root, _| root
                .plugin_tools
                .view_for_mount(tool, &context_a, window_active)
                .is_none()),
            "a failed hide must make the unconfirmed tool unmountable"
        );
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        assert!(
            !gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "a failed hide must not keep rendering the tool content"
        );
    });

    // 渲染路径每帧都会 defer 一次 apply；同一代重复 apply 只能刷新内置活动，
    // 不得再次发起插件 ensure/visibility。这里保留隐藏失败留下的未确认状态，
    // 重复真实 render 入口而不制造新投影：若生命周期门禁只覆盖事件路径，
    // 这些 defer 会立刻重试 visibility_changed 并让计数增长。
    let visibility_before_render_repeat = state
        .visibility_attempts
        .lock()
        .expect("visibility attempt lock")
        .len();
    let generations_before_render_repeat = visual.update(|_, app| {
        root.read_with(app, |root, _| {
            root.plugin_tools
                .attempted_projection_generations_for_test()
        })
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let _ = root.workbench_tool_panels(window, cx);
            let _ = root.workbench_tool_panels(window, cx);
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    assert_eq!(
        state
            .visibility_attempts
            .lock()
            .expect("visibility attempt lock")
            .len(),
        visibility_before_render_repeat,
        "repeated same-generation render applies must not resend plugin visibility"
    );
    assert_eq!(
        visual.update(|_, app| {
            root.read_with(app, |root, _| {
                root.plugin_tools
                    .attempted_projection_generations_for_test()
            })
        }),
        generations_before_render_repeat,
        "repeated same-generation render applies must not mark a new lifecycle attempt"
    );

    // 同一 pane 的重复事件不得隐式重试隐藏失败；显式重试仍是独立新代次。
    let visibility_before_repeat = state
        .visibility_attempts
        .lock()
        .expect("visibility attempt lock")
        .len();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.sync_tool_projection_before_first_draw(window, cx);
        });
    });
    assert_eq!(
        state
            .visibility_attempts
            .lock()
            .expect("visibility attempt lock")
            .len(),
        visibility_before_repeat,
        "the same generation must not retry a failed hide callback"
    );

    // 在触发回调的同一 update 闭包内捕获窗口活动状态：断言必须反映回调触发那一刻
    // 的真实 window_active，闭包外的引用既编译不过也可能与触发时刻不一致。
    let hidden_retry_window_active = visual.update(|window, app| {
        let window_active = window.is_window_active();
        root.update(app, |root, cx| {
            assert!(root.prepare_plugin_tool(tool, window, cx));
            root.sync_tool_projection_before_first_draw(window, cx);
        });
        window_active
    });
    let attempts_after_hidden_retry = state
        .visibility_attempts
        .lock()
        .expect("visibility attempt lock")
        .clone();
    assert_eq!(
        attempts_after_hidden_retry.len(),
        visibility_before_repeat + 1,
        "explicit retry must call visibility_changed again even though the desired value is still hidden"
    );
    assert_eq!(
        attempts_after_hidden_retry.last(),
        Some(&(binding_a.clone(), false, hidden_retry_window_active)),
        "the hidden retry must use the last confirmed target, not a new pane"
    );

    // 重新打开是新的显式操作：即使可见性值回到旧值，也必须重新通知并恢复挂载。
    // 重开回调在本次 update 内同步执行，捕获同一时刻的窗口活动状态供闭包外断言使用。
    let reopen_window_active = visual.update(|window, app| {
        let window_active = window.is_window_active();
        root.update(app, |root, cx| {
            root.right_dock_collapsed = false;
            root.sync_tool_projection_before_first_draw(window, cx);
        });
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        assert!(
            root.read_with(app, |root, _| root
                .plugin_tools
                .view_for_mount(tool, &context_a, window_active)
                .is_some()),
            "reopening after hide retry must remount the retained tool"
        );
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "the reopened first draw must render the retained tool"
        );
        window_active
    });
    assert_eq!(
        state
            .visibility_attempts
            .lock()
            .expect("visibility attempt lock")
            .last(),
        Some(&(binding_a.clone(), true, reopen_window_active)),
        "reopening must explicitly re-confirm visibility instead of trusting the stale value"
    );

    // 真实关闭仍走独立资源门禁：隐藏回调失败后不得让实例永久停留在不可关闭状态。
    state
        .fail_visibility_once
        .lock()
        .expect("visibility failure lock")
        .push(binding_a.clone());
    visual.update(|window, app| {
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        root.update(app, |root, cx| {
            root.right_dock_collapsed = true;
            root.sync_tool_projection_before_first_draw(window, cx);
            assert!(
                root.plugin_tools
                    .view_for_mount(tool, &context_a, window.is_window_active())
                    .is_none(),
                "the failed hide must still leave the tool unmountable before close"
            );
            let deadline = Instant::now() + Duration::from_secs(1);
            let (token, _future) = root
                .plugin_tools
                .begin_close(tool, &context_a, deadline, cx)
                .expect("close must not be blocked by an unconfirmed visibility callback");
            root.plugin_tools
                .complete_close(token, Ok(()))
                .expect("fixture close completion must be accepted");
            root.plugin_tools
                .finalize_close(tool, &context_a)
                .expect("closed fixture instance must release its owner");
            assert_eq!(root.plugin_tools.phase(tool, &context_a), None);
        });
        window.draw(app).clear(app);
        assert!(
            !gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "a closed tool must not render its retained content"
        );
    });
}

/// 内置 AgentLauncher 在窗口失活/激活过渡中不得渲染恢复占位；首帧仍挂载同一 AgentsPanel 实体。
/// 使用生产 render 的可见 fallback 计数取证，覆盖 run_until_parked 后最终快照看不到的首帧。
///
/// @author kongweiguang
#[gpui_kit::test]
fn agent_launcher_builtin_survives_window_activation_round_trip(app: &mut TestAppContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| {
            let (commands, _) =
                root_commands_service::empty_commands_service_with_calls(services.sessions.clone());
            let agent_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let agents =
                Arc::new(agent_runtime.block_on(FixtureAgentService::new(fixture_agent_view())));
            services
                .with_commands(Arc::new(commands))
                .with_agents(agents)
        });
    visual.simulate_resize(size(px(1440.), px(900.)));
    settle_split_fixture(&runtime, visual);
    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            let mut tool_rail = ToolRailSettings::default();
            tool_rail.panel_placements.insert(
                ToolRailToolId::AgentLauncher,
                ToolRailPanelPlacement::Attached,
            );
            root.settings_snapshot = Some(SettingsSnapshot {
                tool_rail,
                ..Default::default()
            });
            root.toggle_tool_rail_panel(ToolRailToolId::AgentLauncher, window, cx);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    let entity_before =
        visual.update(|_, app| root.read(app).agents_panel.as_ref().map(Entity::entity_id));
    assert!(
        entity_before.is_some(),
        "AgentLauncher must mount the production AgentsPanel entity"
    );
    visual.update(|_, app| {
        root.update(app, |root, _| {
            root.plugin_tools.reset_unavailable_fallback_renders()
        });
    });

    visual.deactivate_window();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        assert!(!window.is_window_active());
        let unavailable: gpui_kit::ElementId = "tool-agent-launcher-unavailable".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        assert!(
            !snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&unavailable)),
            "deactivation must not render the AgentLauncher unavailable surface"
        );
    });
    settle_split_fixture(&runtime, visual);
    visual.update(|window, _| window.activate_window());
    visual.run_until_parked();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        assert!(window.is_window_active());
        let unavailable: gpui_kit::ElementId = "tool-agent-launcher-unavailable".into();
        let snapshots = gpui_kit::base::test_support::snapshots(window);
        assert!(
            !snapshots
                .iter()
                .any(|node| node.visible() && node.path().contains(&unavailable)),
            "activation must not render the AgentLauncher unavailable surface"
        );
        let entity_after = root.read(app).agents_panel.as_ref().map(Entity::entity_id);
        assert_eq!(
            entity_after, entity_before,
            "AgentLauncher must keep the same AgentsPanel entity across activation changes"
        );
        let fallback_renders = root
            .read(app)
            .plugin_tools
            .unavailable_fallback_render_count();
        assert_eq!(
            fallback_renders, 0,
            "the AgentLauncher activation transition must never render the unavailable fallback"
        );
    });
}

/// 真实 Root 在分屏焦点切换后按 generation 拒绝迟到的旧 B 回执：B→A→B 后旧票据的
/// 投影值与当前完全相同，仍不得再次应用或重试旧生命周期；只证明无头 App update，不代表原生窗口。
///
/// @author kongweiguang
#[gpui_kit::test]
fn stale_projection_ticket_is_rejected_after_aba_round_trip(app: &mut TestAppContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| {
            let (commands, _) =
                root_commands_service::empty_commands_service_with_calls(services.sessions.clone());
            services.with_commands(Arc::new(commands))
        });
    visual.simulate_resize(size(px(1440.), px(900.)));
    settle_split_fixture(&runtime, visual);

    let tab_id = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let session_a = SessionId::new();
    let session_b = SessionId::new();
    let binding_a = split_binding(session_a.clone(), tab_id.clone(), pane_a.clone(), 1);
    let binding_b = split_binding(session_b.clone(), tab_id.clone(), pane_b.clone(), 2);
    let context_a =
        ToolContext::new(Some(tab_id.clone()), None).with_active_terminal(Some(binding_a.clone()));
    let context_b =
        ToolContext::new(Some(tab_id.clone()), None).with_active_terminal(Some(binding_b.clone()));

    let tool = ToolRailToolId::from_id(SPLIT_TOOL_ID).expect("valid fixture tool id");
    let provider = PluginId::new("fixture.split-provider").expect("valid provider");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    builder.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    builder.register_tool(ToolDescriptor::new(
        ToolId::new(tool.as_str()).expect("valid fixture tool id"),
        provider,
        "Split fixture",
        None,
    ));
    let registry = builder.freeze().expect("valid registry");
    let state = Arc::new(SplitFixtureState::default());
    let created = Arc::new(Mutex::new(None));
    let factory = Arc::new(SplitFixtureFactory {
        state: state.clone(),
        created: created.clone(),
        tool_id: SPLIT_TOOL_ID,
    });

    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            let mut controller = WorkspaceController::from_missing().expect("empty workspace");
            let layout = TerminalLayoutNode::pane(pane_a.clone())
                .split_pane(
                    &pane_a,
                    pane_b.clone(),
                    SplitDirection::Vertical,
                    "split-a-b",
                    false,
                )
                .expect("valid split layout");
            controller
                .synchronize_snapshot(WorkspaceSessionSnapshot {
                    pinned_containers: Vec::new(),
                    active_tab_id: Some(tab_id.clone()),
                    focused_pane_id: Some(pane_a.clone()),
                    selected_machine_id: Some("local".to_owned()),
                    terminal_tabs: vec![WorkspaceSessionTab::Terminal {
                        id: tab_id.clone(),
                        tab_group_id: None,
                        title: "split".to_owned(),
                        machine_id: "local".to_owned(),
                        layout,
                    }],
                    terminal_panes: vec![
                        split_workspace_pane(pane_a.clone()),
                        split_workspace_pane(pane_b.clone()),
                    ],
                    tab_groups: BTreeMap::new(),
                })
                .expect("valid split workspace");
            root.workspace_runtime =
                WorkspaceRuntime::from_snapshot(controller.snapshot()).expect("valid runtime");
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            for (pane, session) in [(&pane_a, session_a.clone()), (&pane_b, session_b.clone())] {
                root.workspace_runtime
                    .install_session(
                        pane,
                        session,
                        split_session_target(),
                        TerminalSize::default(),
                    )
                    .expect("fixture session");
            }
            root.install_pane_input_handler(
                binding_a.clone(),
                split_command_context(&binding_a),
                window,
                cx,
            );
            root.install_pane_input_handler(
                binding_b.clone(),
                split_command_context(&binding_b),
                window,
                cx,
            );
            root.plugin_tools
                .register(&registry, factory.clone())
                .expect("fixture factory");
            let mut tool_rail = ToolRailSettings::default();
            tool_rail
                .panel_placements
                .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Left);
            root.settings_snapshot = Some(SettingsSnapshot {
                tool_rail,
                ..Default::default()
            });
            root.toggle_tool_rail_panel(ToolRailToolId::Logs, window, cx);
            root.toggle_tool_rail_panel(tool, window, cx);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);

    let fixture_view = created
        .lock()
        .expect("fixture entity lock")
        .clone()
        .expect("factory must create the retained view");
    visual.update(|window, app| {
        assert!(
            root.read_with(app, |root, _| root
                .plugin_tools
                .view_for_mount(tool, &context_a, window.is_window_active())
                .is_some()),
            "fixture must start mounted on pane A"
        );
    });

    // 捕获 B 投影的旧票据，再离开 B 并回到 B。旧票据与新投影的值完全相等，
    // 只有 generation 能拒绝它；若仍按值比较，旧 deferred effect 会重新执行。
    let (stale_b_ticket, stale_b_projection) = visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Activate(pane_b.clone()),
                },
                window,
                cx,
            );
            let projection = root
                .plugin_tools
                .current_projection_for_test()
                .expect("pane B must have a staged projection");
            let ticket = root.plugin_tools.stage_projection(projection.clone());
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Activate(pane_a.clone()),
                },
                window,
                cx,
            );
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Activate(pane_b.clone()),
                },
                window,
                cx,
            );
            (ticket, projection)
        })
    });
    let attempts_before_stale = state
        .context_attempts
        .lock()
        .expect("context attempt lock")
        .len();
    let generations_before_stale = visual.update(|_, app| {
        root.read_with(app, |root, _| {
            root.plugin_tools
                .attempted_projection_generations_for_test()
        })
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let current = root
                .plugin_tools
                .current_projection_for_test()
                .expect("pane B must be staged after returning");
            assert_eq!(
                current, stale_b_projection,
                "the stale ticket must match the current value, leaving generation as the only guard"
            );
            assert!(
                !root.plugin_tools.projection_is_current(stale_b_ticket),
                "B→A→B must invalidate the old B ticket even though its value matches again"
            );
            root.apply_tool_projection(stale_b_ticket, stale_b_projection.clone(), window, cx);
        });
        fixture_view.update(app, |_, cx| cx.notify());
        window.draw(app).clear(app);
    });
    assert_eq!(
        state
            .context_attempts
            .lock()
            .expect("context attempt lock")
            .len(),
        attempts_before_stale,
        "a stale deferred ticket must not reach context_changed"
    );
    assert_eq!(
        visual.update(|_, app| {
            root.read_with(app, |root, _| {
                root.plugin_tools
                    .attempted_projection_generations_for_test()
            })
        }),
        generations_before_stale,
        "a stale deferred ticket must not mark a new lifecycle attempt"
    );
    visual.update(|window, app| {
        assert!(
            root.read_with(app, |root, _| root
                .plugin_tools
                .view_for_mount(tool, &context_b, window.is_window_active())
                .is_some()),
            "returning to B must mount the current B target"
        );
        assert!(
            root.read_with(app, |root, _| root
                .plugin_tools
                .view_for_mount(tool, &context_a, window.is_window_active())
                .is_none()),
            "the stale effect must not keep the old A target mountable"
        );
        let live_id: gpui_kit::ElementId = "fixture-split-tool-live".into();
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|node| node.visible() && node.path().contains(&live_id)),
            "the current B projection must stay mounted after rejecting stale B"
        );
    });
}

/// 绘制恰好一帧。断言每次首绘的 Logs 目标与插件上下文都属于当前 Tab，且 A/B 各自保留的
/// 插件实体、Logs 实体和命令实体在切回时身份不变；未装配 AgentService 的 fixture 不覆盖
/// Agent 面板实体与 CLI 生命周期，避免用无关重建路径冒充该证据。
/// 它只证明无头 App update 内的首绘树，不证明真实窗口 present、合成或视觉闪烁。
///
/// @author kongweiguang
#[gpui_kit::test]
fn tab_activation_projects_tools_before_first_draw_and_retains_tab_entities(
    app: &mut TestAppContext,
) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| {
            let (commands, _) =
                root_commands_service::empty_commands_service_with_calls(services.sessions.clone());
            services.with_commands(Arc::new(commands))
        });
    visual.simulate_resize(size(px(1440.), px(900.)));
    settle_split_fixture(&runtime, visual);

    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let session_a = SessionId::new();
    let session_b = SessionId::new();
    let binding_a = split_binding(session_a.clone(), tab_a.clone(), pane_a.clone(), 1);
    let binding_b = split_binding(session_b.clone(), tab_b.clone(), pane_b.clone(), 2);
    let context_a =
        ToolContext::new(Some(tab_a.clone()), None).with_active_terminal(Some(binding_a.clone()));
    let context_b =
        ToolContext::new(Some(tab_b.clone()), None).with_active_terminal(Some(binding_b.clone()));

    let tool = ToolRailToolId::from_id(TAB_TOOL_ID).expect("valid fixture tool id");
    let provider = PluginId::new("fixture.tab-provider").expect("valid provider");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    builder.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    builder.register_tool(ToolDescriptor::new(
        ToolId::new(tool.as_str()).expect("valid fixture tool id"),
        provider,
        "Tab fixture",
        None,
    ));
    let registry = builder.freeze().expect("valid registry");
    let state = Arc::new(SplitFixtureState::default());
    let created = Arc::new(Mutex::new(None));
    let factory = Arc::new(SplitFixtureFactory {
        state: state.clone(),
        created: created.clone(),
        tool_id: TAB_TOOL_ID,
    });

    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            let mut controller = WorkspaceController::from_missing().expect("empty workspace");
            controller
                .synchronize_snapshot(WorkspaceSessionSnapshot {
                    pinned_containers: Vec::new(),
                    active_tab_id: Some(tab_a.clone()),
                    focused_pane_id: Some(pane_a.clone()),
                    selected_machine_id: Some("local".to_owned()),
                    terminal_tabs: vec![
                        WorkspaceSessionTab::Terminal {
                            id: tab_a.clone(),
                            tab_group_id: None,
                            title: "tab-a".to_owned(),
                            machine_id: "local".to_owned(),
                            layout: TerminalLayoutNode::pane(pane_a.clone()),
                        },
                        WorkspaceSessionTab::Terminal {
                            id: tab_b.clone(),
                            tab_group_id: None,
                            title: "tab-b".to_owned(),
                            machine_id: "local".to_owned(),
                            layout: TerminalLayoutNode::pane(pane_b.clone()),
                        },
                    ],
                    terminal_panes: vec![
                        split_workspace_pane(pane_a.clone()),
                        split_workspace_pane(pane_b.clone()),
                    ],
                    tab_groups: BTreeMap::new(),
                })
                .expect("valid two-tab workspace");
            root.workspace_runtime =
                WorkspaceRuntime::from_snapshot(controller.snapshot()).expect("valid runtime");
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            for (pane, session) in [(&pane_a, session_a.clone()), (&pane_b, session_b.clone())] {
                root.workspace_runtime
                    .install_session(
                        pane,
                        session,
                        split_session_target(),
                        TerminalSize::default(),
                    )
                    .expect("fixture session");
            }
            root.install_pane_input_handler(
                binding_a.clone(),
                split_command_context(&binding_a),
                window,
                cx,
            );
            root.install_pane_input_handler(
                binding_b.clone(),
                split_command_context(&binding_b),
                window,
                cx,
            );
            root.plugin_tools
                .register(&registry, factory.clone())
                .expect("fixture factory");
            let mut tool_rail = ToolRailSettings::default();
            tool_rail
                .panel_placements
                .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Left);
            root.settings_snapshot = Some(SettingsSnapshot {
                tool_rail,
                ..Default::default()
            });
            root.toggle_tool_rail_panel(ToolRailToolId::Logs, window, cx);
            root.toggle_tool_rail_panel(tool, window, cx);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);

    let (logs_a, commands_a) = visual.update(|window, app| {
        let logs = root.read_with(app, |root, _| {
            root.logs_view.as_ref().map(|view| view.entity_id())
        });
        let commands = root.read_with(app, |root, _| {
            root.commands_view.as_ref().map(|view| view.entity_id())
        });
        assert!(logs.is_some() && commands.is_some());
        let window_active = window.is_window_active();
        assert!(root.read_with(app, |root, _| {
            root.plugin_tools
                .view_for_mount(tool, &context_a, window_active)
                .is_some()
        }));
        (logs, commands)
    });
    assert_eq!(
        state
            .created_views
            .lock()
            .expect("created views lock")
            .len(),
        1,
        "opening the first Tab must create exactly one Tab-scoped tool instance"
    );
    let a_tool_entity = state.created_views.lock().expect("created views lock")[0].entity_id();

    let switch_and_draw = |visual: &mut gpui_kit::VisualTestContext,
                           target: &TabId,
                           context: &ToolContext,
                           binding: &UiSessionBinding|
     -> (Option<gpui_kit::EntityId>, Option<gpui_kit::EntityId>) {
        let mut result: Option<(Option<gpui_kit::EntityId>, Option<gpui_kit::EntityId>)> = None;
        visual.update(|window, app| {
            state
                .rendered_contexts
                .lock()
                .expect("render context lock")
                .clear();
            root.update(app, |root, cx| {
                assert!(root.handle_workspace_tab_event(
                    &WorkspaceTabEvent::Activate(target.to_owned()),
                    window,
                    cx,
                ));
                root.activate_workspace_tab(target, window, cx);
            });
            let logs_after_event =
                root.read_with(app, |root, _| root.projected_logs_target_for_test.clone());
            assert_eq!(
                logs_after_event
                    .as_ref()
                    .map(|target| target.binding().clone()),
                Some(binding.to_owned()),
                "tab activation must project the target Tab before the first draw"
            );
            let views = state
                .created_views
                .lock()
                .expect("created views lock")
                .clone();
            for view in views {
                view.update(app, |_, cx| cx.notify());
            }
            window.draw(app).clear(app);
            let logs_after_draw =
                root.read_with(app, |root, _| root.projected_logs_target_for_test.clone());
            assert_eq!(
                logs_after_draw
                    .as_ref()
                    .map(|target| target.binding().clone()),
                Some(binding.to_owned()),
                "the first draw must keep the target Tab Logs handoff stable"
            );
            let rendered = state
                .rendered_contexts
                .lock()
                .expect("render context lock")
                .clone();
            assert!(
                !rendered.is_empty()
                    && rendered.iter().all(|rendered| {
                        rendered.as_ref().is_some_and(|rendered| {
                            rendered.active_terminal() == context.active_terminal()
                        })
                    }),
                "the first draw after tab activation must render only the current Tab target"
            );
            let window_active = window.is_window_active();
            let (mount_current, mount_other) = root.read_with(app, |root, _| {
                let other = if context.active_tab() == Some(target) {
                    if target == &tab_a {
                        &context_b
                    } else {
                        &context_a
                    }
                } else {
                    &context_a
                };
                (
                    root.plugin_tools
                        .view_for_mount(tool, context, window_active)
                        .is_some(),
                    root.plugin_tools
                        .view_for_mount(tool, other, window_active)
                        .is_some(),
                )
            });
            assert!(
                mount_current && !mount_other,
                "the first draw must mount only the current Tab tool instance"
            );
            // result 的 Option 外层只用于跨闭包携带"是否完成"的哨兵，实体对本身始终存在。
            result = Some(root.read_with(app, |root, _| {
                (
                    root.logs_view.as_ref().map(|view| view.entity_id()),
                    root.commands_view.as_ref().map(|view| view.entity_id()),
                )
            }));
        });

        result.expect("switch draw completed")
    };

    let (logs_b, commands_b) = switch_and_draw(visual, &tab_b, &context_b, &binding_b);
    assert_ne!(logs_a, logs_b, "each Tab must keep its own Logs entity");
    assert_ne!(
        commands_a, commands_b,
        "each Tab must keep its own commands entity"
    );
    assert_eq!(
        state
            .created_views
            .lock()
            .expect("created views lock")
            .len(),
        2,
        "switching to B must create only B's Tab-scoped tool instance"
    );

    let (logs_a_again, commands_a_again) = switch_and_draw(visual, &tab_a, &context_a, &binding_a);
    assert_eq!(
        logs_a_again, logs_a,
        "returning to A must restore A's original Logs entity"
    );
    assert_eq!(
        commands_a_again, commands_a,
        "returning to A must restore A's original commands entity"
    );
    assert_eq!(
        state
            .created_views
            .lock()
            .expect("created views lock")
            .len(),
        2,
        "returning to A must reuse the retained Tab tool entity instead of rebuilding it"
    );
    assert_eq!(
        state.created_views.lock().expect("created views lock")[0].entity_id(),
        a_tool_entity,
        "the original A tool entity must retain its identity"
    );
    settle_split_fixture(&runtime, visual);
}

/// Tab 内容已由先前 reconcile 切到新标签，但本次激活 request_focus=false、没有 pane 焦点事件时，
/// 不能把“缓存标签恰好是当前标签”误判成投影已同步。事件结束后、deferred effect 排空前只绘制
/// 一帧，断言首绘已拒绝旧 Tab 的 Logs/插件目标并挂载当前 Tab 的实例。
/// 它只覆盖无头首绘树，不证明原生 present 或桌面合成。
///
/// @author kongweiguang
#[gpui_kit::test]
fn tab_activation_without_pane_focus_projects_before_first_draw_when_contents_are_current(
    app: &mut TestAppContext,
) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| {
            let (commands, _) =
                root_commands_service::empty_commands_service_with_calls(services.sessions.clone());
            services.with_commands(Arc::new(commands))
        });
    visual.simulate_resize(size(px(1440.), px(900.)));
    settle_split_fixture(&runtime, visual);

    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let session_a = SessionId::new();
    let session_b = SessionId::new();
    let binding_a = split_binding(session_a.clone(), tab_a.clone(), pane_a.clone(), 1);
    let binding_b = split_binding(session_b.clone(), tab_b.clone(), pane_b.clone(), 2);
    let context_a =
        ToolContext::new(Some(tab_a.clone()), None).with_active_terminal(Some(binding_a.clone()));
    let context_b =
        ToolContext::new(Some(tab_b.clone()), None).with_active_terminal(Some(binding_b.clone()));

    let tool = ToolRailToolId::from_id(TAB_TOOL_ID).expect("valid fixture tool id");
    let provider = PluginId::new("fixture.tab-provider").expect("valid provider");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    builder.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    builder.register_tool(ToolDescriptor::new(
        ToolId::new(tool.as_str()).expect("valid fixture tool id"),
        provider,
        "Tab fixture",
        None,
    ));
    let registry = builder.freeze().expect("valid registry");
    let state = Arc::new(SplitFixtureState::default());
    let created = Arc::new(Mutex::new(None));
    let factory = Arc::new(SplitFixtureFactory {
        state: state.clone(),
        created: created.clone(),
        tool_id: TAB_TOOL_ID,
    });

    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            let mut controller = WorkspaceController::from_missing().expect("empty workspace");
            controller
                .synchronize_snapshot(WorkspaceSessionSnapshot {
                    pinned_containers: Vec::new(),
                    active_tab_id: Some(tab_a.clone()),
                    focused_pane_id: Some(pane_a.clone()),
                    selected_machine_id: Some("local".to_owned()),
                    terminal_tabs: vec![
                        WorkspaceSessionTab::Terminal {
                            id: tab_a.clone(),
                            tab_group_id: None,
                            title: "tab-a".to_owned(),
                            machine_id: "local".to_owned(),
                            layout: TerminalLayoutNode::pane(pane_a.clone()),
                        },
                        WorkspaceSessionTab::Terminal {
                            id: tab_b.clone(),
                            tab_group_id: None,
                            title: "tab-b".to_owned(),
                            machine_id: "local".to_owned(),
                            layout: TerminalLayoutNode::pane(pane_b.clone()),
                        },
                    ],
                    terminal_panes: vec![
                        split_workspace_pane(pane_a.clone()),
                        split_workspace_pane(pane_b.clone()),
                    ],
                    tab_groups: BTreeMap::new(),
                })
                .expect("valid two-tab workspace");
            root.workspace_runtime =
                WorkspaceRuntime::from_snapshot(controller.snapshot()).expect("valid runtime");
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            for (pane, session) in [(&pane_a, session_a.clone()), (&pane_b, session_b.clone())] {
                root.workspace_runtime
                    .install_session(
                        pane,
                        session,
                        split_session_target(),
                        TerminalSize::default(),
                    )
                    .expect("fixture session");
            }
            root.install_pane_input_handler(
                binding_a.clone(),
                split_command_context(&binding_a),
                window,
                cx,
            );
            root.install_pane_input_handler(
                binding_b.clone(),
                split_command_context(&binding_b),
                window,
                cx,
            );
            root.plugin_tools
                .register(&registry, factory.clone())
                .expect("fixture factory");
            let mut tool_rail = ToolRailSettings::default();
            tool_rail
                .panel_placements
                .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Left);
            root.settings_snapshot = Some(SettingsSnapshot {
                tool_rail,
                ..Default::default()
            });
            root.toggle_tool_rail_panel(ToolRailToolId::Logs, window, cx);
            root.toggle_tool_rail_panel(tool, window, cx);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);

    visual.update(|window, app| {
        let window_active = window.is_window_active();
        state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clear();
        root.update(app, |root, cx| {
            assert!(root.tab_tool_contents_are_current());
            assert!(root.handle_workspace_tab_event(
                &WorkspaceTabEvent::Activate(tab_b.clone()),
                window,
                cx,
            ));
            // 先让内容缓存切到 B，制造“缓存 current 但没有经过 pane 焦点同步”的状态。
            root.reconcile_tab_tool_contents(window, cx);
            assert!(root.tab_tool_contents_are_current());
            assert_eq!(
                root.projected_logs_target_for_test
                    .as_ref()
                    .map(|target| target.binding().clone()),
                Some(binding_a.clone()),
                "the fixture must start from A's committed projection"
            );
            assert!(
                root.plugin_tools
                    .view_for_mount(tool, &context_a, window_active)
                    .is_some()
            );
            assert!(
                root.plugin_tools
                    .view_for_mount(tool, &context_b, window_active)
                    .is_none()
            );

            root.activate_workspace_tab_surface(&tab_b, false, window, cx);
            assert_eq!(
                root.projected_logs_target_for_test
                    .as_ref()
                    .map(|target| target.binding().clone()),
                Some(binding_b.clone()),
                "request_focus=false must still project the current Tab before the first draw"
            );
            assert!(
                root.plugin_tools
                    .view_for_mount(tool, &context_b, window_active)
                    .is_some()
            );
            assert!(
                root.plugin_tools
                    .view_for_mount(tool, &context_a, window_active)
                    .is_none()
            );
        });
        let views = state
            .created_views
            .lock()
            .expect("created views lock")
            .clone();
        for view in views {
            view.update(app, |_, cx| cx.notify());
        }
        window.draw(app).clear(app);

        let logs_after_draw =
            root.read_with(app, |root, _| root.projected_logs_target_for_test.clone());
        assert_eq!(
            logs_after_draw
                .as_ref()
                .map(|target| target.binding().clone()),
            Some(binding_b.clone()),
            "the first draw must keep the current Tab Logs handoff stable"
        );
        let rendered = state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .clone();
        assert!(
            !rendered.is_empty()
                && rendered.iter().all(|rendered| {
                    rendered.as_ref().is_some_and(|rendered| {
                        rendered.active_terminal() == context_b.active_terminal()
                    })
                }),
            "the first draw must render only the current Tab plugin context"
        );
    });
    settle_split_fixture(&runtime, visual);
}

/// 真实 Root 投影把 SFTP 活动状态绑定到停靠可见性和窗口激活，而不是实体是否存在。
/// 折叠或失焦时停用缓存的 SFTP 面板，恢复可见后重新激活同一实体，不重建或触发目录 I/O。
///
/// @author kongweiguang
#[gpui_kit::test]
fn sftp_panel_activity_tracks_dock_and_window_visibility(app: &mut TestAppContext) {
    let (runtime, _file_runtime, root, visual) = sftp_activity_fixture(app);
    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            root.toggle_tool_rail_panel(ToolRailToolId::Sftp, window, cx);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    let pane = visual
        .update(|_, app| root.read_with(app, |root, _| root.sftp_panel.clone()))
        .expect("SFTP panel is created when its tool is visible");
    assert!(
        visual.update(|_, app| pane.read(app).is_active_for_test()),
        "visible SFTP panel must be active"
    );

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.right_dock_collapsed = true;
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    assert!(
        !visual.update(|_, app| pane.read(app).is_active_for_test()),
        "collapsing the right dock must deactivate the cached SFTP panel"
    );

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.right_dock_collapsed = false;
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    assert!(
        visual.update(|_, app| pane.read(app).is_active_for_test()),
        "restoring the dock must reactivate the same SFTP entity"
    );

    visual.deactivate_window();
    visual.update(|window, app| {
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    assert!(
        !visual.update(|_, app| pane.read(app).is_active_for_test()),
        "an inactive window must stop driving the SFTP entrance"
    );

    visual.update(|window, app| {
        window.activate_window();
        window.draw(app).clear(app);
    });
    settle_split_fixture(&runtime, visual);
    assert!(
        visual.update(|_, app| pane.read(app).is_active_for_test()),
        "window activation must restore SFTP activity without a new entity"
    );
}

/// SFTP 右栏回归需要真实 FilesPane；在隔离 Root 装配上注入内存文件/传输端口和本地初始端点，
/// 让 `sftp_panel` 在构造期真实创建，而不是依赖插件门禁或事后补挂。
/// 文件服务使用独立 runtime，调用方需在测试期间持有，避免队列 worker 随 fixture 提前销毁。
///
/// @author kongweiguang
fn sftp_activity_fixture(
    app: &mut TestAppContext,
) -> (
    tokio::runtime::Runtime,
    tokio::runtime::Runtime,
    Entity<crate::root::RootView>,
    &mut gpui_kit::VisualTestContext,
) {
    let (file_runtime, files, transfers, _, _) =
        crate::features::files::action_test_support::services();
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, move |mut services| {
            services.files = Some(files);
            services.transfers = Some(transfers);
            services.initial_file_endpoint = Some(FileEndpoint::Local {
                path: "C:/sftp-activity-fixture".into(),
            });
            services
        });
    (runtime, file_runtime, root, visual)
}

/// 真实 Root 的 fixture 初始化包含偏好与工作区回执；交替推进 GPUI 与 Tokio 后再断言。
///
/// @author kongweiguang
fn settle_split_fixture(
    runtime: &tokio::runtime::Runtime,
    visual: &mut gpui_kit::VisualTestContext,
) {
    for _ in 0..5 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
}

/// 绑定身份必须同时匹配会话、Tab 与 pane，右键工具上下文才不会落到别的分屏。
///
/// @author kongweiguang
fn split_binding(
    session_id: SessionId,
    tab_id: TabId,
    pane_id: PaneId,
    session_generation: u64,
) -> UiSessionBinding {
    UiSessionBinding {
        session_id,
        owner: SessionOwner::Ui { tab_id, pane_id },
        session_generation,
    }
}

/// 输入 handler 安装会核对 binding 与命令上下文的三组身份一致；
/// 这里只提供 let-else 校验所需的稳定内存值，不启动真实 shell。
///
/// @author kongweiguang
fn split_command_context(binding: &UiSessionBinding) -> TerminalCommandContext {
    TerminalCommandContext {
        session_id: binding.session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation,
        prompt_epoch: 0,
        input_generation: 0,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: false,
        prompt_line_empty: false,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::PromptNotReady,
        ),
    }
}

/// 工作区快照使用用户可见本地 pane，恢复目标与 runtime 安装目标保持同一形态。
///
/// @author kongweiguang
fn split_workspace_pane(id: PaneId) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id,
        title: "shell".to_owned(),
        machine_id: "local".to_owned(),
        mode: WorkspacePaneMode::Local,
        target: Some(WorkspaceTargetRef::Local { profile_id: None }),
        remote_host_id: None,
        container_id: None,
        profile_id: None,
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Online,
        latency_ms: None,
        local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
        output_history: None,
    }
}

/// 本地目标只需满足 runtime 的持久目标核对，测试不会向端口发起真实命令。
///
/// @author kongweiguang
fn split_session_target() -> SessionTarget {
    SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("fixture-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    })
}

/// 实例、视图和测试观察点共享同一提交状态；render 只在真实挂载时采样宿主已接受的上下文。
///
/// @author kongweiguang
#[derive(Default)]
struct SplitFixtureState {
    renders: AtomicUsize,
    committed_context: Mutex<Option<ToolContext>>,
    rendered_contexts: Mutex<Vec<Option<ToolContext>>>,
    committed_history: Mutex<Vec<ToolContext>>,
    /// 每个 Tab 作用域实例只创建一次；切回旧 Tab 时用于识别实体是否被重建。
    created_views: Mutex<Vec<Entity<SplitFixtureView>>>,
    /// 指定终端上的上下文回调必须失败，用来验证失败路径仍保持严格挂载门禁。
    fail_terminals: Mutex<Vec<UiSessionBinding>>,
    failed_contexts: Mutex<Vec<ToolContext>>,
    /// 每次真实 context_changed 调用都记录，用来证明同代重复事件没有隐式重试。
    context_attempts: Mutex<Vec<ToolContext>>,
    /// 每次真实 visibility_changed 调用记录目标、可见性和窗口活动状态。
    visibility_attempts: Mutex<Vec<(UiSessionBinding, bool, bool)>>,
    /// 只让指定终端的下一次显隐回调失败，随后同一显式重试必须能成功恢复。
    fail_visibility_once: Mutex<Vec<UiSessionBinding>>,
}

/// 真实保留实体带有可观测正文；渲染计数用于区分“仍在元素树中”与上一帧快照残留。
///
/// @author kongweiguang
struct SplitFixtureView {
    state: Arc<SplitFixtureState>,
    /// 每个 Tab 实例只观察自己收到的上下文，避免切回旧 Tab 时被另一实例的最后提交污染。
    committed_context: Option<ToolContext>,
}

impl Render for SplitFixtureView {
    /// 只有宿主实际包含该实体时才执行 render；未挂载期间不采样也不增加计数。
    /// 这里读取的是该实例已成功提交的上下文，而不是当前 controller 或查询时刻的状态。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.state.renders.fetch_add(1, Ordering::SeqCst);
        self.state
            .rendered_contexts
            .lock()
            .expect("render context lock")
            .push(self.committed_context.clone());
        div()
            .size_full()
            .id("fixture-split-tool-live")
            .test_support()
            .child("split tool live")
    }
}

/// Tab 作用域使 A/B 共享同一实例键；上下文变化必须通过 context_changed 更新目标。
///
/// @author kongweiguang
struct SplitFixtureInstance {
    view: Entity<SplitFixtureView>,
    state: Arc<SplitFixtureState>,
    /// 记录插件已成功接受的终端；显隐失败时不能把它改成另一个目标。
    binding: UiSessionBinding,
}

impl ToolInstance for SplitFixtureInstance {
    /// 每帧借用同一实体句柄，验证宿主没有重建视图。
    ///
    /// @author kongweiguang
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 只有宿主接受的成功回调才更新视图可见的提交上下文；失败时保留旧状态，
    /// 让宿主继续保持拒绝挂载，而不是让测试状态单方面宣称目标已切换。
    ///
    /// @author kongweiguang
    fn context_changed(&mut self, context: &ToolContext, cx: &mut App) -> AppResult<()> {
        self.state
            .context_attempts
            .lock()
            .expect("context attempt lock")
            .push(context.clone());
        // 指定终端返回失败，模拟插件拒绝切换；宿主不能因此回退到旧目标可用状态。
        if self
            .state
            .fail_terminals
            .lock()
            .expect("failure lock")
            .iter()
            .any(|binding| context.active_terminal() == Some(binding))
        {
            self.state
                .failed_contexts
                .lock()
                .expect("failure history lock")
                .push(context.clone());
            return Err(AppError::infrastructure("fixture split context failure"));
        }
        if let Some(binding) = context.active_terminal() {
            self.binding = binding.clone();
        }
        *self
            .state
            .committed_context
            .lock()
            .expect("committed context lock") = Some(context.clone());
        self.state
            .committed_history
            .lock()
            .expect("context history lock")
            .push(context.clone());
        self.view.update(cx, |view, cx| {
            view.committed_context = Some(context.clone());
            cx.notify();
        });
        Ok(())
    }

    /// 只消费一次失败注入；重复事件若没有重新发起回调，失败历史不会增长，
    /// 而显式重试必须再次调用同一目标并在成功后恢复挂载。
    ///
    /// @author kongweiguang
    fn visibility_changed(
        &mut self,
        visible: bool,
        window_active: bool,
        _: &mut App,
    ) -> AppResult<()> {
        self.state
            .visibility_attempts
            .lock()
            .expect("visibility attempt lock")
            .push((self.binding.clone(), visible, window_active));
        let should_fail = {
            let mut failures = self
                .state
                .fail_visibility_once
                .lock()
                .expect("visibility failure lock");
            failures
                .iter()
                .position(|binding| binding == &self.binding)
                .map(|index| failures.remove(index))
                .is_some()
        };
        if should_fail {
            return Err(AppError::infrastructure("fixture split visibility failure"));
        }
        Ok(())
    }

    /// 关闭回执立即完成，测试只覆盖挂载过渡不涉及资源回收时序。
    ///
    /// @author kongweiguang
    fn begin_close(&mut self, _: Instant, _: &mut App) -> AppResult<ToolCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

/// 分屏用例与工厂共用的贡献身份；集中成常量避免目录声明与 `ToolFactory::tool_id`
/// 两处字符串各自演化，注册门禁才能在同一次 `register` 中通过。
///
/// @author kongweiguang
const SPLIT_TOOL_ID: &str = "fixture.split-tool";

/// Tab 用例的贡献身份，必须与 `Tab fixture` 目录声明和工厂返回值完全一致。
///
/// @author kongweiguang
const TAB_TOOL_ID: &str = "fixture.tab-tool";

/// 工厂保留创建出的实体句柄，测试可在切换帧强制其重绘并比较渲染计数。
///
/// @author kongweiguang
struct SplitFixtureFactory {
    state: Arc<SplitFixtureState>,
    created: Arc<Mutex<Option<Entity<SplitFixtureView>>>>,
    /// 冻结目录与工厂必须使用同一贡献身份：`PluginTools::register` 只接受目录里已声明的
    /// 工具，固定写死一个 id 会让 Tab 用例声明的 `fixture.tab-tool` 在注册阶段直接失败，
    /// 测试就再也走不到投影断言。身份由用例显式传入，分屏与 Tab 用例各用自己的声明。
    tool_id: &'static str,
}

impl ToolFactory for SplitFixtureFactory {
    /// 冻结目录与工厂使用同一稳定身份。
    ///
    /// @author kongweiguang
    fn tool_id(&self) -> &str {
        self.tool_id
    }

    /// 使用真实 Kit 图标，使测试进入与普通插件工具相同的 Rail 宿主。
    ///
    /// @author kongweiguang
    fn presentation(&self) -> ToolPresentation {
        ToolPresentation::new(
            "Split fixture",
            "Split fixture",
            gpui_kit::assets::IconName::FileText,
        )
    }

    /// 活动终端变化只在 Tab 作用域内更新同一实例，才能覆盖分屏切换路径。
    ///
    /// @author kongweiguang
    fn scope(&self) -> ToolScope {
        ToolScope::Tab
    }

    /// 创建时窗口已装配，实体将在后续整个分屏切换中保持同一身份。
    /// 创建上下文即宿主首次接受的上下文，测试据此区分 A 与后续切换。
    ///
    /// @author kongweiguang
    fn create(
        &self,
        context: &ToolContext,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        *self
            .state
            .committed_context
            .lock()
            .expect("committed context lock") = Some(context.clone());
        let view = cx.new(|_| SplitFixtureView {
            state: self.state.clone(),
            committed_context: Some(context.clone()),
        });
        self.state
            .created_views
            .lock()
            .expect("created views lock")
            .push(view.clone());
        *self.created.lock().expect("created lock") = Some(view.clone());
        Ok(Box::new(SplitFixtureInstance {
            view,
            state: self.state.clone(),
            binding: context
                .active_terminal()
                .cloned()
                .expect("fixture create context must carry a terminal binding"),
        }))
    }
}
