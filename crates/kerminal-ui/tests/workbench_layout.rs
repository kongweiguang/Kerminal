//! 工作台响应式预算的外部契约测试。
//!
//! @author kongweiguang

#[path = "../src/root/workbench_layout.rs"]
mod workbench_layout;

use kerminal_application::preferences::ToolRailToolId;
use workbench_layout::{WorkbenchLayoutInput, resolve_workbench_layout};

/// 正常窗口保留用户侧栏与右栏尺寸，并让中心终端使用产品最小可读宽度。
///
/// @author kongweiguang
#[test]
fn expanded_layout_keeps_user_panel_widths_and_center_minimum() {
    let open_tools = [ToolRailToolId::AgentLauncher];
    let input = WorkbenchLayoutInput {
        viewport_width: 1440.0,
        viewport_height: 900.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: false,
        host_sidebar_preferred_width: 224.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: false,
        attached_tool_open: true,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 24.0,
        open_tool_order: &open_tools,
    };

    let plan = resolve_workbench_layout(&input);

    assert!(!plan.compact_width);
    assert!(!plan.compact_panels);
    assert_eq!(plan.center_min_width, 320.0);
    assert_eq!(input.host_sidebar_preferred_width, 224.0);
    assert_eq!(input.attached_dock_preferred_width, 420.0);
}

/// 左工具、主机侧栏、右 Dock 与底部工具同时打开时，抽屉投影使用最后活动工具而保留偏好输入。
///
/// @author kongweiguang
#[test]
fn narrow_layout_projects_last_open_tool_without_mutating_preferences() {
    let open_tools = [
        ToolRailToolId::AgentLauncher,
        ToolRailToolId::Context,
        ToolRailToolId::Logs,
    ];
    let input = WorkbenchLayoutInput {
        viewport_width: 820.0,
        viewport_height: 800.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: false,
        host_sidebar_preferred_width: 384.0,
        attached_dock_preferred_width: 604.0,
        left_tool_open: true,
        attached_tool_open: true,
        bottom_tool_open: true,
        focused_tools_height: 180.0,
        focused_tools_expanded_height: 180.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 32.0,
        open_tool_order: &open_tools,
    };

    let plan = resolve_workbench_layout(&input);

    assert!(plan.compact_width);
    assert!(plan.compact_panels);
    assert!(plan.effective_host_sidebar_collapsed);
    assert_eq!(plan.compact_active_tool, Some(ToolRailToolId::Logs));
    assert_eq!(plan.center_min_width, 240.0);
    assert_eq!(input.host_sidebar_preferred_width, 384.0);
    assert_eq!(input.attached_dock_preferred_width, 604.0);
    assert_eq!(input.open_tool_order, open_tools.as_slice());
}

/// 极窄窗口只压缩中心目标宽度，验证主机轨与固定区预算仍留出可见终端面积。
///
/// @author kongweiguang
#[test]
fn very_narrow_window_keeps_center_terminal_visible() {
    let open_tools = [ToolRailToolId::AgentLauncher];
    let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
        viewport_width: 280.0,
        viewport_height: 720.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: true,
        host_sidebar_preferred_width: 224.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: true,
        attached_tool_open: true,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 32.0,
        open_tool_order: &open_tools,
    });

    assert!(plan.compact_width);
    assert!(plan.center_min_width > 0.0);
    assert!(plan.center_min_width + 64.0 <= 280.0);
    assert_eq!(
        plan.compact_active_tool,
        Some(ToolRailToolId::AgentLauncher)
    );
    assert_eq!(plan.compact_drawer_left_inset, 8.0);
}

/// 右 Dock 的用户折叠偏好不应丢失其打开工具身份，紧凑抽屉仍可提供该面板的显式入口。
///
/// @author kongweiguang
#[test]
fn compact_projection_keeps_collapsed_attached_tool_in_open_order() {
    let open_tools = [ToolRailToolId::Context, ToolRailToolId::AgentLauncher];
    let input = WorkbenchLayoutInput {
        viewport_width: 700.0,
        viewport_height: 720.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: true,
        host_sidebar_preferred_width: 224.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: true,
        attached_tool_open: true,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 24.0,
        open_tool_order: &open_tools,
    };

    let plan = resolve_workbench_layout(&input);

    assert!(plan.compact_panels);
    assert_eq!(
        plan.compact_active_tool,
        Some(ToolRailToolId::AgentLauncher)
    );
    assert!(input.right_dock_collapsed);
    assert_eq!(input.open_tool_order, open_tools.as_slice());
}

/// 移除26px状态栏后用644px窗口保持原670px窗口的正文高度预算，使底部面板仍低于180px可读门槛。
///
/// @author kongweiguang
#[test]
fn short_window_caps_bottom_panel_before_terminal_area() {
    let open_tools = [ToolRailToolId::Logs];
    let input = WorkbenchLayoutInput {
        viewport_width: 1200.0,
        viewport_height: 644.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: true,
        host_sidebar_preferred_width: 224.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: false,
        attached_tool_open: false,
        bottom_tool_open: true,
        focused_tools_height: 180.0,
        focused_tools_expanded_height: 180.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 32.0,
        open_tool_order: &open_tools,
    };

    let plan = resolve_workbench_layout(&input);

    assert!(plan.compact_panels);
    assert!(!plan.compact_focused_tools);
    assert_eq!(plan.bottom_panel_max_height, 159.0);
    assert_eq!(plan.compact_active_tool, Some(ToolRailToolId::Logs));
}

/// 500px 短窗把多条恢复诊断限高滚动、合计预留205px时，临时收起焦点工具并保留偏好输入。
///
/// @author kongweiguang
#[test]
fn short_window_projects_focused_tools_without_changing_user_collapse_state() {
    let input = WorkbenchLayoutInput {
        viewport_width: 1200.0,
        viewport_height: 500.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: true,
        host_sidebar_preferred_width: 228.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: false,
        attached_tool_open: false,
        bottom_tool_open: false,
        focused_tools_height: 180.0,
        focused_tools_expanded_height: 180.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 205.0,
        open_tool_order: &[],
    };

    let plan = resolve_workbench_layout(&input);

    assert!(plan.compact_focused_tools);
    assert!(plan.compact_panels);
    assert_eq!(plan.compact_drawer_left_inset, 236.0);
    assert_eq!(input.focused_tools_height, 180.0);
    assert_eq!(input.reserved_vertical_height, 205.0);
}

/// 用户已折叠工具栏时仍用展开高度决定首击走临时抽屉，同时底栏预算只扣除当前可见高度。
///
/// @author kongweiguang
#[test]
fn collapsed_tools_use_temporary_drawer_on_first_click_when_terminal_space_is_tight() {
    let input = WorkbenchLayoutInput {
        viewport_width: 1200.0,
        viewport_height: 500.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: true,
        host_sidebar_preferred_width: 228.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: false,
        attached_tool_open: false,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 180.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 205.0,
        open_tool_order: &[],
    };

    let plan = resolve_workbench_layout(&input);

    assert!(plan.compact_focused_tools);
    assert_eq!(plan.bottom_panel_max_height, 31.0);
    assert_eq!(input.focused_tools_height, 0.0);
    assert_eq!(input.focused_tools_expanded_height, 180.0);
}

/// 中心仍可容纳展开工具栏时，折叠态的首次点击继续使用用户偏好开关。
///
/// @author kongweiguang
#[test]
fn collapsed_tools_keep_persistent_expand_path_when_terminal_space_is_available() {
    let input = WorkbenchLayoutInput {
        viewport_width: 1200.0,
        viewport_height: 500.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: true,
        host_sidebar_preferred_width: 228.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: false,
        attached_tool_open: false,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 180.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 0.0,
        open_tool_order: &[],
    };

    let plan = resolve_workbench_layout(&input);

    assert!(!plan.compact_focused_tools);
    assert_eq!(plan.bottom_panel_max_height, 236.0);
    assert_eq!(input.focused_tools_height, 0.0);
}

/// 用户手动收起左栏后，紧凑宽度预算不保留旧缩略轨，工具抽屉从正文内边距起绘。
///
/// @author kongweiguang
#[test]
fn manually_collapsed_sidebar_releases_width_from_compact_budget() {
    let input = WorkbenchLayoutInput {
        viewport_width: 340.0,
        viewport_height: 720.0,
        host_sidebar_collapsed: true,
        right_dock_collapsed: true,
        host_sidebar_preferred_width: 320.0,
        attached_dock_preferred_width: 420.0,
        left_tool_open: false,
        attached_tool_open: false,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 32.0,
        open_tool_order: &[],
    };

    let plan = resolve_workbench_layout(&input);

    assert!(plan.compact_width);
    assert!(plan.effective_host_sidebar_collapsed);
    assert_eq!(plan.center_min_width, 240.0);
    assert_eq!(plan.compact_drawer_left_inset, 8.0);
}
