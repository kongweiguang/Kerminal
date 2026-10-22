//! 工作台响应式布局预算；只计算当前窗口的有效视图，不回写用户的停靠偏好。
//!
//! @author kongweiguang

use kerminal_application::preferences::ToolRailToolId;

const BODY_FIXED_WIDTH: f32 = 64.0;
const BODY_HORIZONTAL_PADDING: f32 = 8.0;
const BODY_SPLITTER_WIDTH: f32 = 4.0;
const HOST_MIN_WIDTH_WITH_SPLITTER: f32 = 172.0;
const CENTER_EXPANDED_MIN_WIDTH: f32 = 320.0;
const CENTER_COMPACT_TARGET_WIDTH: f32 = 240.0;
const LEFT_TOOL_WIDTH_WITH_GUTTER: f32 = 328.0;
const ATTACHED_DOCK_MIN_WIDTH_WITH_GUTTER: f32 = 268.0;
const TITLE_BAR_HEIGHT: f32 = 36.0;
const BODY_BOTTOM_PADDING: f32 = 8.0;
const BOTTOM_SPLITTER_HEIGHT: f32 = 9.0;
pub(super) const CENTER_MIN_HEIGHT: f32 = 220.0;
pub(super) const BOTTOM_TOOL_MIN_HEIGHT: f32 = 180.0;
const BOTTOM_TOOL_MAX_FRACTION: f32 = 0.55;

/// 只携带预算需要的事实；宽高偏好按只读借用传入，响应式投影不能改写它们。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct WorkbenchLayoutInput<'a> {
    pub(super) viewport_width: f32,
    pub(super) viewport_height: f32,
    pub(super) host_sidebar_collapsed: bool,
    pub(super) right_dock_collapsed: bool,
    pub(super) host_sidebar_preferred_width: f32,
    pub(super) attached_dock_preferred_width: f32,
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
        finite_non_negative(input.host_sidebar_preferred_width).max(HOST_MIN_WIDTH_WITH_SPLITTER)
    };
    let attached_dock_visible = input.attached_tool_open && !input.right_dock_collapsed;
    let attached_dock_width = if attached_dock_visible {
        finite_non_negative(input.attached_dock_preferred_width)
            .max(ATTACHED_DOCK_MIN_WIDTH_WITH_GUTTER)
    } else {
        BODY_SPLITTER_WIDTH
    };
    let left_tool_width = if input.left_tool_open {
        LEFT_TOOL_WIDTH_WITH_GUTTER
    } else {
        0.0
    };
    let expanded_required_width = host_width
        + CENTER_EXPANDED_MIN_WIDTH
        + left_tool_width
        + attached_dock_width
        + BODY_FIXED_WIDTH;
    let compact_width = viewport_width < expanded_required_width;

    // 主机栏收起时完全卸载，紧凑中心只扣除仍实际存在的右侧固定区。
    let compact_center_budget = (viewport_width - BODY_FIXED_WIDTH).max(0.0);
    let center_min_width = if compact_width {
        compact_center_budget.min(CENTER_COMPACT_TARGET_WIDTH)
    } else {
        CENTER_EXPANDED_MIN_WIDTH
    };

    let reserved_height = finite_non_negative(input.reserved_vertical_height);
    let focused_tools_height = finite_non_negative(input.focused_tools_height);
    let focused_tools_expanded_height = finite_non_negative(input.focused_tools_expanded_height);
    let available_center_stack = (viewport_height
        - TITLE_BAR_HEIGHT
        - finite_non_negative(input.status_bar_height)
        - BODY_BOTTOM_PADDING
        - reserved_height)
        .max(0.0);
    let bottom_splitter_height = if input.bottom_tool_open {
        BOTTOM_SPLITTER_HEIGHT
    } else {
        0.0
    };
    let bottom_panel_max_height = (available_center_stack
        - CENTER_MIN_HEIGHT
        - focused_tools_height
        - bottom_splitter_height)
        .max(0.0)
        .min(viewport_height * BOTTOM_TOOL_MAX_FRACTION);
    let compact_height = input.bottom_tool_open && bottom_panel_max_height < BOTTOM_TOOL_MIN_HEIGHT;
    let compact_focused_tools = focused_tools_expanded_height > 0.0
        && available_center_stack < CENTER_MIN_HEIGHT + focused_tools_expanded_height;
    let compact_panels = compact_width || compact_height || compact_focused_tools;
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
    }
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
