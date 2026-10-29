//! 工作台响应式布局预算；只计算当前窗口的有效视图，不回写用户的停靠偏好。
//!
//! @author kongweiguang

use kerminal_application::preferences::ToolRailToolId;

const BODY_FIXED_WIDTH: f32 = 64.0;
const BODY_HORIZONTAL_PADDING: f32 = 8.0;
pub(super) const TOOL_RAIL_WIDTH: f32 = 44.0;
pub(super) const RESIZABLE_EDGE_GUTTER: f32 = 4.0;
/// BODY_FIXED_WIDTH already includes the rail, so panel budgets reserve only the remaining shell edges.
const NON_RAIL_FIXED_WIDTH: f32 = BODY_FIXED_WIDTH - TOOL_RAIL_WIDTH;
const COMPACT_WIDTH_BREAKPOINT: f32 = 900.0;
const CENTER_EXPANDED_MIN_WIDTH: f32 = 360.0;
const CENTER_COMPACT_TARGET_WIDTH: f32 = 360.0;
pub(super) const TOOL_PANEL_MIN_TOTAL_WIDTH: f32 = 300.0;
pub(super) const ATTACHED_DOCK_CONTENT_MIN_WIDTH: f32 =
    TOOL_PANEL_MIN_TOTAL_WIDTH - TOOL_RAIL_WIDTH;
const TOOL_PANEL_INITIAL_MIN_WIDTH: f32 = 340.0;
const TOOL_PANEL_INITIAL_MAX_WIDTH: f32 = 444.0;
const TITLE_BAR_HEIGHT: f32 = 36.0;
const BODY_BOTTOM_PADDING: f32 = 8.0;
pub(super) const CENTER_MIN_HEIGHT: f32 = 220.0;
pub(super) const BOTTOM_TOOL_MIN_HEIGHT: f32 = 180.0;
const BOTTOM_TOOL_MAX_FRACTION: f32 = 0.55;
const FLOATING_PANEL_MAX_WIDTH: f32 = 760.0;
const FLOATING_PANEL_MAX_HEIGHT: f32 = 720.0;
const FLOATING_PANEL_HORIZONTAL_INSET: f32 = 8.0;
const FLOATING_PANEL_VERTICAL_INSET: f32 = 8.0;

/// Floating center panels retain integer logical-pixel positions to keep the enclosing state comparable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FloatingPanelPosition {
    pub(super) x: i32,
    pub(super) y: i32,
}

/// Main-shell dimensions are expressed relative to the body beneath its 36px title bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FloatingPanelGeometry {
    pub(super) width: f32,
    pub(super) height: f32,
}

/// 只携带预算需要的事实；宽高偏好按只读借用传入，响应式投影不能改写它们。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct WorkbenchLayoutInput<'a> {
    pub(super) viewport_width: f32,
    pub(super) viewport_height: f32,
    pub(super) host_sidebar_collapsed: bool,
    pub(super) right_dock_collapsed: bool,
    pub(super) host_sidebar_preferred_width: f32,
    /// GPUI right pane outer width, including its 4px gutter and excluding the 44px rail.
    pub(super) attached_dock_preferred_width: f32,
    pub(super) left_tool_panel_preferred_width: f32,
    pub(super) left_tool_open: bool,
    pub(super) attached_tool_open: bool,
    pub(super) bottom_tool_open: bool,
    /// 当前工具区的可见占用高度，供底部面板预算使用。
    pub(super) focused_tools_height: f32,
    /// 若用户请求展开时的实际工具区高度，供终端空间是否需要抽屉的判定使用。
    pub(super) focused_tools_expanded_height: f32,
    pub(super) status_bar_height: f32,
    pub(super) reserved_vertical_height: f32,
    pub(super) open_tool_order: &'a [ToolRailToolId],
}

/// 响应式状态只决定挂载位置与可用预算；用户的折叠标记、工具顺序和面板尺寸保持原值。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WorkbenchLayoutPlan {
    pub(super) compact_width: bool,
    pub(super) compact_panels: bool,
    /// 展开终端工具会压缩中心区时，首击应走临时抽屉而不是改写用户偏好。
    pub(super) compact_focused_tools: bool,
    pub(super) effective_host_sidebar_collapsed: bool,
    pub(super) compact_active_tool: Option<ToolRailToolId>,
    pub(super) compact_drawer_left_inset: f32,
    pub(super) center_min_width: f32,
    pub(super) bottom_panel_max_height: f32,
    /// Right pane content width; callers add its resize gutter when configuring the GPUI pane.
    pub(super) attached_dock_max_width: f32,
    pub(super) left_tool_panel_max_width: f32,
}

/// 用真实可见项和用户尺寸预算侧栏组合；完全关闭的主机侧栏不保留缩略轨或分隔槽。
/// 只有无法同时保住中心终端时才投影为抽屉，折叠布局预算也不为已隐藏侧栏预留宽度。
///
/// @author kongweiguang
pub(super) fn resolve_workbench_layout(input: &WorkbenchLayoutInput<'_>) -> WorkbenchLayoutPlan {
    let viewport_width = finite_non_negative(input.viewport_width);
    let viewport_height = finite_non_negative(input.viewport_height);
    let host_width = if input.host_sidebar_collapsed {
        0.0
    } else {
        finite_non_negative(input.host_sidebar_preferred_width)
    };
    let attached_dock_visible = input.attached_tool_open && !input.right_dock_collapsed;
    let left_tool_width = if input.left_tool_open {
        finite_non_negative(input.left_tool_panel_preferred_width)
    } else {
        0.0
    };
    let compact_width = viewport_width < COMPACT_WIDTH_BREAKPOINT;

    // 主机栏收起时完全卸载，紧凑中心只扣除仍实际存在的右侧固定区。
    let compact_center_budget = (viewport_width - BODY_FIXED_WIDTH).max(0.0);
    let center_min_width = if compact_width {
        compact_center_budget.min(CENTER_COMPACT_TARGET_WIDTH)
    } else {
        CENTER_EXPANDED_MIN_WIDTH
    };

    let reserved_height = finite_non_negative(input.reserved_vertical_height);
    let focused_tools_expanded_height = finite_non_negative(input.focused_tools_expanded_height);
    let available_center_stack = (viewport_height
        - TITLE_BAR_HEIGHT
        - finite_non_negative(input.status_bar_height)
        - BODY_BOTTOM_PADDING
        - reserved_height)
        .max(0.0);
    let shell_body_height = (viewport_height - TITLE_BAR_HEIGHT).max(0.0);
    let bottom_panel_max_height = BOTTOM_TOOL_MIN_HEIGHT.max(
        (shell_body_height * BOTTOM_TOOL_MAX_FRACTION)
            .min(shell_body_height - TITLE_BAR_HEIGHT - CENTER_MIN_HEIGHT),
    );
    let compact_focused_tools = focused_tools_expanded_height > 0.0
        && available_center_stack < CENTER_MIN_HEIGHT + focused_tools_expanded_height;
    let compact_panels = compact_width;
    let compact_active_tool = compact_panels
        .then(|| input.open_tool_order.iter().next_back().copied())
        .flatten();
    let effective_host_sidebar_collapsed = input.host_sidebar_collapsed || compact_width;
    // 抽屉相对 workbench_body 定位；完全收起的主机栏不占空间，仅保留正文内边距。
    let compact_drawer_left_inset = BODY_HORIZONTAL_PADDING
        + if effective_host_sidebar_collapsed {
            0.0
        } else {
            host_width
        };

    WorkbenchLayoutPlan {
        compact_width,
        compact_panels,
        compact_focused_tools,
        effective_host_sidebar_collapsed,
        compact_active_tool,
        compact_drawer_left_inset,
        center_min_width,
        bottom_panel_max_height,
        attached_dock_max_width: main_attached_dock_total_to_gpui_content_width(
            resolve_tool_panel_max_width(
                viewport_width,
                host_width + left_tool_width + NON_RAIL_FIXED_WIDTH,
                0.0,
            ),
        ),
        left_tool_panel_max_width: resolve_tool_panel_max_width(
            viewport_width,
            host_width + NON_RAIL_FIXED_WIDTH,
            if attached_dock_visible {
                gpui_attached_dock_panel_width_to_main_total(input.attached_dock_preferred_width)
            } else {
                0.0
            },
        ),
    }
}

/// Match the desktop tool-panel default while leaving a minimum terminal width beside the panel.
pub(super) fn initial_tool_panel_width(viewport_width: f32) -> f32 {
    (finite_non_negative(viewport_width) * 0.24)
        .clamp(TOOL_PANEL_INITIAL_MIN_WIDTH, TOOL_PANEL_INITIAL_MAX_WIDTH)
}

/// Convert main's total right-dock width into GPUI content width; the frame gutter is added separately.
///
/// @author kongweiguang
pub(super) fn main_attached_dock_total_to_gpui_content_width(main_total_width: f32) -> f32 {
    finite_non_negative(main_total_width - TOOL_RAIL_WIDTH)
}

/// Convert a GPUI outer width back to main's total width after removing its frame gutter.
///
/// @author kongweiguang
pub(super) fn gpui_attached_dock_panel_width_to_main_total(panel_width: f32) -> f32 {
    finite_non_negative(panel_width - RESIZABLE_EDGE_GUTTER) + TOOL_RAIL_WIDTH
}

/// Configure the GPUI right pane by adding its frame gutter after converting main's content width.
///
/// @author kongweiguang
pub(super) fn main_attached_dock_total_to_gpui_panel_width(main_total_width: f32) -> f32 {
    main_attached_dock_total_to_gpui_content_width(main_total_width) + RESIZABLE_EDGE_GUTTER
}

/// Resolve main's total panel width before callers project a right dock into GPUI content units.
pub(super) fn resolve_tool_panel_max_width(
    viewport_width: f32,
    left_columns_width: f32,
    other_panel_width: f32,
) -> f32 {
    let viewport_width = finite_non_negative(viewport_width);
    let midpoint_width = viewport_width * 0.5;
    let terminal_safe_width = viewport_width
        - finite_non_negative(left_columns_width)
        - finite_non_negative(other_panel_width)
        - CENTER_EXPANDED_MIN_WIDTH;
    TOOL_PANEL_MIN_TOTAL_WIDTH.max(midpoint_width.min(terminal_safe_width))
}

/// Size the floating workbench surface to the main shell's max geometry and visible margins.
pub(super) fn floating_panel_geometry(
    viewport_width: f32,
    body_height: f32,
) -> FloatingPanelGeometry {
    FloatingPanelGeometry {
        width: (finite_non_negative(viewport_width) - FLOATING_PANEL_HORIZONTAL_INSET * 4.0)
            .clamp(0.0, FLOATING_PANEL_MAX_WIDTH),
        height: (finite_non_negative(body_height) - FLOATING_PANEL_VERTICAL_INSET * 3.0)
            .clamp(0.0, FLOATING_PANEL_MAX_HEIGHT),
    }
}

/// Center first appearance and every restored position inside the Shell without hiding its title bar.
pub(super) fn clamp_floating_panel_position(
    position: FloatingPanelPosition,
    viewport_width: f32,
    body_height: f32,
    geometry: FloatingPanelGeometry,
) -> FloatingPanelPosition {
    let min_x = FLOATING_PANEL_HORIZONTAL_INSET;
    let min_y = FLOATING_PANEL_VERTICAL_INSET;
    let max_x = (finite_non_negative(viewport_width) - geometry.width - min_x).max(min_x);
    let max_y = (finite_non_negative(body_height) - geometry.height - min_y).max(min_y);
    FloatingPanelPosition {
        x: (position.x as f32).clamp(min_x, max_x).round() as i32,
        y: (position.y as f32).clamp(min_y, max_y).round() as i32,
    }
}

/// The shell body starts below the 36px title bar, so its centered start keeps main's 8px top margin.
pub(super) fn initial_floating_panel_position(
    viewport_width: f32,
    body_height: f32,
    geometry: FloatingPanelGeometry,
) -> FloatingPanelPosition {
    clamp_floating_panel_position(
        FloatingPanelPosition {
            x: ((finite_non_negative(viewport_width) - geometry.width) / 2.0).round() as i32,
            y: ((finite_non_negative(body_height) - geometry.height
                + FLOATING_PANEL_VERTICAL_INSET)
                / 2.0)
                .round() as i32,
        },
        viewport_width,
        body_height,
        geometry,
    )
}

/// 窗口测量尚未稳定时把非有限值归零，避免负预算进入 GPUI 的尺寸范围。
///
/// @author kongweiguang
fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}
