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
        left_tool_panel_preferred_width: 324.0,
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
    assert_eq!(plan.center_min_width, 360.0);
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
        left_tool_panel_preferred_width: 324.0,
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
    assert_eq!(plan.center_min_width, 360.0);
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
        left_tool_panel_preferred_width: 324.0,
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
    assert_eq!(plan.center_min_width, 216.0);
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
        left_tool_panel_preferred_width: 324.0,
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

/// 短窗底部面板按 main 的 Shell 正文 55% 上限与终端预留空间取较小值。
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
        left_tool_panel_preferred_width: 324.0,
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

    assert!(!plan.compact_panels);
    assert!(!plan.compact_focused_tools);
    assert_eq!(plan.bottom_panel_max_height, 334.4);
    assert_eq!(plan.compact_active_tool, None);
}

/// 纵向预算不足时临时收起焦点工具，但 500px 高度本身不改变水平停靠布局或偏好。
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
        left_tool_panel_preferred_width: 324.0,
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
    assert!(!plan.compact_panels);
    assert_eq!(plan.compact_drawer_left_inset, 236.0);
    assert_eq!(input.focused_tools_height, 180.0);
    assert_eq!(input.reserved_vertical_height, 205.0);
}

/// 用户已折叠工具栏时仍按展开高度决定首击走临时抽屉，底部上限由 Shell 高度独立计算。
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
        left_tool_panel_preferred_width: 324.0,
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
    assert_eq!(plan.bottom_panel_max_height, 208.0);
    assert_eq!(input.focused_tools_height, 0.0);
    assert_eq!(input.focused_tools_expanded_height, 180.0);
}

/// 中心仍可容纳展开工具栏时，折叠态首次点击沿用用户开关，底部高度仍按 main 短窗上限计算。
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
        left_tool_panel_preferred_width: 324.0,
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
    assert_eq!(plan.bottom_panel_max_height, 208.0);
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
        left_tool_panel_preferred_width: 324.0,
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
    assert_eq!(plan.center_min_width, 276.0);
    assert_eq!(plan.compact_drawer_left_inset, 8.0);
}

/// main 的紧凑布局断点按逻辑像素 900px 判定，并只影响窗口视图投影。
///
/// @author kongweiguang
#[test]
fn compact_breakpoint_is_900_logical_pixels() {
    let mut input = WorkbenchLayoutInput {
        viewport_width: 899.0,
        viewport_height: 720.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: false,
        host_sidebar_preferred_width: 224.0,
        attached_dock_preferred_width: 420.0,
        left_tool_panel_preferred_width: 324.0,
        left_tool_open: false,
        attached_tool_open: false,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 0.0,
        open_tool_order: &[],
    };

    assert!(resolve_workbench_layout(&input).compact_width);
    input.viewport_width = 900.0;
    assert!(!resolve_workbench_layout(&input).compact_width);
}

/// 首次工具面板宽度保留 340–444px 的可读范围，窗口异常测量按零宽回退。
///
/// @author kongweiguang
#[test]
fn initial_tool_panel_width_clamps_default_size_and_ratio() {
    assert_eq!(workbench_layout::initial_tool_panel_width(1000.0), 340.0);
    assert_eq!(workbench_layout::initial_tool_panel_width(1500.0), 360.0);
    assert_eq!(workbench_layout::initial_tool_panel_width(2500.0), 444.0);
    assert_eq!(workbench_layout::initial_tool_panel_width(-10.0), 340.0);
    assert_eq!(workbench_layout::initial_tool_panel_width(f32::NAN), 340.0);
}

/// 右 pane 限宽按 main 总宽投影一次；左 pane 预算反向补回右侧 rail 后再扣终端最小宽。
///
/// @author kongweiguang
#[test]
fn attached_dock_budget_maps_main_total_through_rail_and_gutter_once() {
    let open_tools = [ToolRailToolId::AgentLauncher, ToolRailToolId::Snippets];
    let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
        viewport_width: 2000.0,
        viewport_height: 900.0,
        host_sidebar_collapsed: false,
        right_dock_collapsed: false,
        host_sidebar_preferred_width: 220.0,
        attached_dock_preferred_width: 956.0,
        left_tool_panel_preferred_width: 324.0,
        left_tool_open: true,
        attached_tool_open: true,
        bottom_tool_open: false,
        focused_tools_height: 0.0,
        focused_tools_expanded_height: 0.0,
        status_bar_height: 0.0,
        reserved_vertical_height: 0.0,
        open_tool_order: &open_tools,
    });

    assert_eq!(plan.attached_dock_max_width, 956.0);
    assert_eq!(plan.left_tool_panel_max_width, 404.0);
}

/// main 总宽只减 Rail 得到 GPUI 内容宽，外框单独加入 gutter，确保最小值和往返映射一致。
///
/// @author kongweiguang
#[test]
fn attached_dock_minimum_and_main_width_round_trip_keep_units_explicit() {
    let content_minimum = workbench_layout::ATTACHED_DOCK_CONTENT_MIN_WIDTH;
    let main_minimum = workbench_layout::TOOL_PANEL_MIN_TOTAL_WIDTH;
    let minimum_panel_width =
        workbench_layout::main_attached_dock_total_to_gpui_panel_width(main_minimum);

    assert_eq!(content_minimum, 256.0);
    assert_eq!(
        workbench_layout::main_attached_dock_total_to_gpui_content_width(main_minimum),
        content_minimum
    );
    assert_eq!(minimum_panel_width, 260.0);
    assert_eq!(
        workbench_layout::gpui_attached_dock_panel_width_to_main_total(minimum_panel_width),
        main_minimum
    );

    for main_total_width in [340.0, 360.0, 444.0] {
        let panel_width =
            workbench_layout::main_attached_dock_total_to_gpui_panel_width(main_total_width);
        assert_eq!(
            panel_width,
            main_total_width - workbench_layout::TOOL_RAIL_WIDTH
                + workbench_layout::RESIZABLE_EDGE_GUTTER
        );
        assert_eq!(
            panel_width - workbench_layout::RESIZABLE_EDGE_GUTTER,
            workbench_layout::main_attached_dock_total_to_gpui_content_width(main_total_width)
        );
        assert_eq!(
            workbench_layout::gpui_attached_dock_panel_width_to_main_total(panel_width),
            main_total_width
        );
    }
}

/// 浮窗几何保留可见内边距，并把大窗尺寸限制在产品上限内。
///
/// @author kongweiguang
#[test]
fn floating_panel_geometry_applies_insets_and_size_caps() {
    assert_eq!(
        workbench_layout::floating_panel_geometry(640.0, 500.0),
        workbench_layout::FloatingPanelGeometry {
            width: 608.0,
            height: 476.0,
        }
    );
    assert_eq!(
        workbench_layout::floating_panel_geometry(1200.0, 1000.0),
        workbench_layout::FloatingPanelGeometry {
            width: 760.0,
            height: 720.0,
        }
    );
    assert_eq!(
        workbench_layout::floating_panel_geometry(20.0, 10.0),
        workbench_layout::FloatingPanelGeometry {
            width: 0.0,
            height: 0.0,
        }
    );
}

/// 首次浮窗位置居中，恢复位置则夹紧到窗口可见范围以免标题栏移出工作区。
///
/// @author kongweiguang
#[test]
fn floating_panel_positions_center_and_clamp_inside_viewport() {
    let geometry = workbench_layout::floating_panel_geometry(1200.0, 900.0);
    assert_eq!(
        workbench_layout::initial_floating_panel_position(1200.0, 900.0, geometry),
        workbench_layout::FloatingPanelPosition { x: 220, y: 94 }
    );

    assert_eq!(
        workbench_layout::clamp_floating_panel_position(
            workbench_layout::FloatingPanelPosition { x: -100, y: 2000 },
            1200.0,
            900.0,
            geometry,
        ),
        workbench_layout::FloatingPanelPosition { x: 8, y: 172 }
    );
    assert_eq!(
        workbench_layout::clamp_floating_panel_position(
            workbench_layout::FloatingPanelPosition { x: 2000, y: -100 },
            1200.0,
            900.0,
            geometry,
        ),
        workbench_layout::FloatingPanelPosition { x: 432, y: 8 }
    );
}
