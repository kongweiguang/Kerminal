//! 原生工作台的三栏布局组合；所有持久会话与 feature entity 仍由 Root 持有。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, time::Duration};

use gpui_kit::{
    AnyElement, App, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    StatefulInteractiveElement as _, Window,
    assets::IconName,
    base::Selectable as _,
    canvas,
    component::{
        Disableable, FocusTrapElement as _, Icon, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants},
        resizable::{h_resizable, resizable_panel, v_resizable},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px, relative,
};
use kerminal_application::{
    preferences::{
        InterfaceDensity, SettingsSnapshot, ToolRailPanelPlacement, ToolRailSettings,
        ToolRailToolId, WorkbenchLayoutSettings,
    },
    workspace::{WorkspaceSessionSnapshot, WorkspaceSessionTab},
};
use kerminal_domain::identity::{PaneId, TabId};

use crate::{features::connections::HostNavigation, theme::ThemeTokens};

#[cfg(target_os = "windows")]
use super::windows_title_bar::windows_workbench_title_bar;
use super::{
    CenterPanel, RootConfirmationOwner, RootView, logical_input_modal_blocked, workbench_drawers,
    workbench_layout, workspace_files::WorkspaceFileRestoreState,
};
#[cfg(not(target_os = "windows"))]
use gpui_kit::component::TitleBar;

const HOST_NAVIGATION_WIDTH: f32 = 220.0;
const HOST_NAVIGATION_CONTROL_WIDTH: f32 = 40.0;
const HOST_NAVIGATION_MIN_WIDTH: f32 = 168.0;
const HOST_NAVIGATION_MAX_WIDTH: f32 = 360.0;
const WORKBENCH_MIN_HEIGHT: f32 = 240.0;
const LEFT_TOOL_PANEL_MIN_WIDTH: f32 = 300.0;
const ATTACHED_DOCK_MAX_WIDTH: f32 = 1_360.0;
const WORKBENCH_PANEL_GUTTER: f32 = 4.0;
const WORKBENCH_TITLE_BAR_HEIGHT: f32 = 36.0;

pub(super) use super::workbench_layout::{RESIZABLE_EDGE_GUTTER, TOOL_RAIL_WIDTH};

/// 键盘操作冻结当前绘制帧的浮窗与边界，窗口变化后由下一帧重新提供一致几何。
#[derive(Clone, Copy)]
pub(super) struct FloatingPanelKeyLayout {
    rendered_position: workbench_layout::FloatingPanelPosition,
    viewport_width: f32,
    body_height: f32,
    geometry: workbench_layout::FloatingPanelGeometry,
}

/// Root 只保留最新 RAM 意图与 owner 序号；revision、在途与失败 fence 仍由 Preferences 管理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingWorkbenchLayout {
    pub(super) request_id: u64,
    pub(super) layout: WorkbenchLayoutSettings,
}

/// 测量值是逻辑 px；侧栏/Dock 去掉自身 4px 槽，底部直接传零，不接受非有限或越界尺寸。
///
/// @author kongweiguang
pub(super) fn workbench_content_dimension(
    measured: gpui_kit::Pixels,
    gutter: f32,
    minimum: u16,
    maximum: u16,
) -> Option<u16> {
    let content = measured.as_f32() - gutter;
    (content.is_finite() && (f32::from(minimum)..=f32::from(maximum)).contains(&content))
        .then(|| content.round() as u16)
}

/// 外部快照可推进确认基线，但待办手势必须等自己序号的收据，不能被旧快照恢复覆盖。
///
/// @author kongweiguang
pub(super) fn should_restore_workbench_layout(
    previous: Option<&WorkbenchLayoutSettings>,
    incoming: &WorkbenchLayoutSettings,
    has_pending_intent: bool,
) -> bool {
    !has_pending_intent && previous != Some(incoming)
}

/// owner 已完成仍可能有待派发的末序号收据；退出必须同时等两个只读投影归零。
///
/// @author kongweiguang
pub(super) fn workbench_layout_receipts_complete(
    pending: &Option<PendingWorkbenchLayout>,
    owner_has_pending: bool,
) -> bool {
    pending.is_none() && !owner_has_pending
}

/// 旧成功也更新确认基线，避免普通 ToolRail 保存携带旧布局；只有末序号能清掉 RAM 待办。
///
/// @author kongweiguang
fn accept_workbench_layout_baseline(
    settings: &mut Option<SettingsSnapshot>,
    pending: &mut Option<PendingWorkbenchLayout>,
    request_id: u64,
    layout: WorkbenchLayoutSettings,
) -> bool {
    if let Some(settings) = settings {
        settings.workbench_layout = layout;
    }
    let accepted = pending
        .as_ref()
        .is_some_and(|intent| intent.request_id == request_id);
    if accepted {
        *pending = None;
    }
    accepted
}

/// 入口主区与贴底区遵循全局顺序；打开的隐藏项仍留在主区以便用户关闭。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ToolRailEntries {
    pub(super) main: Vec<ToolRailToolId>,
    pub(super) bottom: Vec<ToolRailToolId>,
}

/// 每个停靠方向最多挂载一个面板，Entity 生命周期由 Root 继续持有。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ToolRailPanelSlots {
    pub(super) attached: Option<ToolRailToolId>,
    pub(super) left: Option<ToolRailToolId>,
    pub(super) bottom: Option<ToolRailToolId>,
    pub(super) center: Option<ToolRailToolId>,
}

/// 工作台只消费已归一的入口与面板宿主，不负责创建或销毁业务 Entity。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ToolRailLayout {
    pub(super) entries: ToolRailEntries,
    pub(super) panels: ToolRailPanelSlots,
}

/// 配置保留历史 ToolId；Context 已不作为产品面板展示。
pub(super) struct WorkbenchToolPanelRenderers {
    pub(super) agent_launcher: AnyElement,
    pub(super) sftp: AnyElement,
    pub(super) containers: AnyElement,
    pub(super) snippets: AnyElement,
    pub(super) tmux: AnyElement,
    pub(super) ports: AnyElement,
    pub(super) system: AnyElement,
    pub(super) logs: AnyElement,
}

impl WorkbenchToolPanelRenderers {
    /// 按领域工具身份取出唯一 renderer；意外重复挂载时显示错误而不重复消费 Entity。
    ///
    /// @author kongweiguang
    fn take_for(&mut self, tool: ToolRailToolId) -> AnyElement {
        let replacement = repeated_tool_panel_renderer(tool);
        let renderer = match tool {
            ToolRailToolId::Context => return replacement,
            ToolRailToolId::AgentLauncher => &mut self.agent_launcher,
            ToolRailToolId::Sftp => &mut self.sftp,
            ToolRailToolId::Containers => &mut self.containers,
            ToolRailToolId::Snippets => &mut self.snippets,
            ToolRailToolId::Tmux => &mut self.tmux,
            ToolRailToolId::Ports => &mut self.ports,
            ToolRailToolId::System => &mut self.system,
            ToolRailToolId::Logs => &mut self.logs,
        };
        std::mem::replace(renderer, replacement)
    }
}

/// Root 按 ToolId 为每个业务面板提供唯一 renderer，工作台只负责选宿主。
pub(super) struct WorkbenchToolPanels {
    pub(super) layout: ToolRailLayout,
    pub(super) renderers: WorkbenchToolPanelRenderers,
}

/// 保存当前 Attached 工具与右栏共享宽度；内容身份由各工具的 retained Entity 负责。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolRailPanelState {
    open_order: Vec<ToolRailToolId>,
    active_tab_id: Option<TabId>,
    tab_open_orders: BTreeMap<Option<TabId>, Vec<ToolRailToolId>>,
    tab_right_dock_collapsed: BTreeMap<Option<TabId>, bool>,
    floating_positions: BTreeMap<ToolRailToolId, workbench_layout::FloatingPanelPosition>,
    floating_drag: Option<FloatingPanelDrag>,
    viewport_width: u16,
    attached_tool: Option<ToolRailToolId>,
    attached_default_width: Option<gpui_kit::Pixels>,
    shared_attached_width: Option<gpui_kit::Pixels>,
}

/// A drag uses the original pointer and panel position so every move is independent of event order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FloatingPanelDrag {
    tool: ToolRailToolId,
    pointer_origin_x: i32,
    pointer_origin_y: i32,
    panel_origin: workbench_layout::FloatingPanelPosition,
}

impl Default for ToolRailPanelState {
    /// 默认不占用终端右侧宽度；可见工具仍在 Rail 中可达，打开后才挂载唯一面板实体。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            open_order: Vec::new(),
            active_tab_id: None,
            tab_open_orders: BTreeMap::new(),
            tab_right_dock_collapsed: BTreeMap::new(),
            floating_positions: BTreeMap::new(),
            floating_drag: None,
            viewport_width: 1_440,
            attached_tool: None,
            attached_default_width: None,
            shared_attached_width: None,
        }
    }
}

impl ToolRailPanelState {
    /// 新 Tab 继承切换时的工具集合；已访问 Tab 即使集合为空也优先恢复自身快照。
    /// 收起状态同样保留显式 false，只有从未初始化的新 Tab 才继承当前状态；Tab 切换同时终止旧浮窗拖动。
    ///
    /// @author kongweiguang
    pub(super) fn activate_tab(
        &mut self,
        tab_id: Option<TabId>,
        live_tab_ids: &[TabId],
        right_dock_collapsed: bool,
    ) -> Option<bool> {
        if self.active_tab_id == tab_id {
            self.tab_open_orders.retain(|cached_tab_id, _| {
                cached_tab_id
                    .as_ref()
                    .is_none_or(|cached_tab_id| live_tab_ids.contains(cached_tab_id))
            });
            self.tab_right_dock_collapsed.retain(|cached_tab_id, _| {
                cached_tab_id
                    .as_ref()
                    .is_none_or(|cached_tab_id| live_tab_ids.contains(cached_tab_id))
            });
            return None;
        }

        self.end_floating_drag();
        self.tab_right_dock_collapsed
            .insert(self.active_tab_id.clone(), right_dock_collapsed);
        let previous_open_order = std::mem::take(&mut self.open_order);
        self.tab_open_orders
            .insert(self.active_tab_id.clone(), previous_open_order.clone());
        self.active_tab_id = tab_id.clone();
        self.open_order = self
            .tab_open_orders
            .remove(&tab_id)
            .unwrap_or(previous_open_order);
        let restored_collapsed = self
            .tab_right_dock_collapsed
            .remove(&tab_id)
            .unwrap_or(right_dock_collapsed);
        self.tab_open_orders.retain(|cached_tab_id, _| {
            cached_tab_id
                .as_ref()
                .is_none_or(|cached_tab_id| live_tab_ids.contains(cached_tab_id))
        });
        self.tab_right_dock_collapsed.retain(|cached_tab_id, _| {
            cached_tab_id
                .as_ref()
                .is_none_or(|cached_tab_id| live_tab_ids.contains(cached_tab_id))
        });
        Some(restored_collapsed)
    }

    /// Remember the latest logical viewport so non-render callbacks can restore the same panel baseline.
    pub(super) fn remember_viewport_width(&mut self, viewport_width: f32) {
        if viewport_width.is_finite() {
            self.viewport_width =
                viewport_width.max(0.0).round().clamp(0.0, u16::MAX as f32) as u16;
        }
    }

    /// Return the last finite logical width used to derive main-shell panel defaults.
    pub(super) fn viewport_width(&self) -> f32 {
        f32::from(self.viewport_width)
    }

    /// Read the per-tool run-time floating position without persisting it to application settings.
    pub(super) fn floating_position(
        &self,
        tool: ToolRailToolId,
    ) -> Option<workbench_layout::FloatingPanelPosition> {
        self.floating_positions.get(&tool).copied()
    }

    /// Clamp and remember a floating position after first render or viewport resizing.
    pub(super) fn set_floating_position(
        &mut self,
        tool: ToolRailToolId,
        position: workbench_layout::FloatingPanelPosition,
    ) {
        self.floating_positions.insert(tool, position);
    }

    /// Start moving only the captured floating tool, using a stable drag origin for all later events.
    pub(super) fn begin_floating_drag(
        &mut self,
        tool: ToolRailToolId,
        pointer_x: f32,
        pointer_y: f32,
        panel_origin: workbench_layout::FloatingPanelPosition,
    ) {
        self.floating_drag = Some(FloatingPanelDrag {
            tool,
            pointer_origin_x: pointer_x.round() as i32,
            pointer_origin_y: pointer_y.round() as i32,
            panel_origin,
        });
    }

    /// Move the captured tool within its current geometry; unchanged coordinates do not rerender the workspace.
    pub(super) fn update_floating_drag(
        &mut self,
        pointer_x: f32,
        pointer_y: f32,
        viewport_width: f32,
        body_height: f32,
        geometry: workbench_layout::FloatingPanelGeometry,
    ) -> bool {
        let Some(drag) = self.floating_drag else {
            return false;
        };
        let position = workbench_layout::clamp_floating_panel_position(
            workbench_layout::FloatingPanelPosition {
                x: drag.panel_origin.x + pointer_x.round() as i32 - drag.pointer_origin_x,
                y: drag.panel_origin.y + pointer_y.round() as i32 - drag.pointer_origin_y,
            },
            viewport_width,
            body_height,
            geometry,
        );
        if self.floating_positions.get(&drag.tool) == Some(&position) {
            return false;
        }
        self.floating_positions.insert(drag.tool, position);
        true
    }

    /// Move the floating panel with a paired keyboard delta using the same clamped runtime geometry as pointer dragging.
    pub(super) fn nudge_floating_position(
        &mut self,
        tool: ToolRailToolId,
        rendered_position: workbench_layout::FloatingPanelPosition,
        delta: (i32, i32),
        viewport_width: f32,
        body_height: f32,
        geometry: workbench_layout::FloatingPanelGeometry,
    ) -> bool {
        let origin = self
            .floating_positions
            .get(&tool)
            .copied()
            .unwrap_or(rendered_position);
        let position = workbench_layout::clamp_floating_panel_position(
            workbench_layout::FloatingPanelPosition {
                x: origin.x.saturating_add(delta.0),
                y: origin.y.saturating_add(delta.1),
            },
            viewport_width,
            body_height,
            geometry,
        );
        if position == origin {
            return false;
        }
        self.floating_positions.insert(tool, position);
        true
    }

    /// Finish a captured drag after mouse release or window focus loss, preserving its last clamped position.
    pub(super) fn finish_floating_drag(&mut self) -> bool {
        self.end_floating_drag().is_some()
    }

    /// Escape cancels a floating drag by restoring the captured origin before the panel closes.
    pub(super) fn cancel_floating_drag(&mut self) -> bool {
        let Some(drag) = self.end_floating_drag() else {
            return false;
        };
        self.floating_positions.insert(drag.tool, drag.panel_origin);
        true
    }

    /// End the data-only drag snapshot so layout state remains cloneable and comparable.
    ///
    /// @author kongweiguang
    fn end_floating_drag(&mut self) -> Option<FloatingPanelDrag> {
        self.floating_drag.take()
    }

    /// The paint phase installs global pointer listeners only while a floating drag is active.
    pub(super) fn floating_drag_is_active(&self) -> bool {
        self.floating_drag.is_some()
    }

    /// 切换指定工具；历史 Context 只保留配置可读性，不再创建面板状态。
    /// 同方向最近打开者替换旧宿主，不同方向保持并行；被替换工具的未完成拖动立即失效。
    ///
    /// @author kongweiguang
    pub(super) fn toggle(&mut self, settings: &ToolRailSettings, tool: ToolRailToolId) {
        self.reproject(settings);
        if tool == ToolRailToolId::Context {
            return;
        }
        if self.open_order.contains(&tool) {
            self.open_order.retain(|candidate| *candidate != tool);
            return;
        }

        let placement = panel_placement(settings, tool);
        self.open_order
            .retain(|candidate| panel_placement(settings, *candidate) != placement);
        self.open_order.push(tool);
        if self
            .floating_drag
            .is_some_and(|drag| !self.open_order.contains(&drag.tool))
        {
            self.end_floating_drag();
        }
    }

    /// 显式关闭只移除原工具的运行时打开事实；重复回调不能反向打开，也不改停靠位置或用户宽度，并结束该工具拖动。
    ///
    /// @author kongweiguang
    pub(super) fn close(&mut self, settings: &ToolRailSettings, tool: ToolRailToolId) -> bool {
        self.reproject(settings);
        let was_open = self.open_order.contains(&tool);
        self.open_order.retain(|candidate| *candidate != tool);
        if self.floating_drag.is_some_and(|drag| drag.tool == tool) {
            self.end_floating_drag();
        }
        was_open
    }

    /// 将已打开的工具提到抽屉前台；只调整窗口会话顺序，不改变该工具的停靠配置。
    ///
    /// @author kongweiguang
    pub(super) fn activate(&mut self, tool: ToolRailToolId) {
        if let Some(index) = self
            .open_order
            .iter()
            .position(|candidate| *candidate == tool)
        {
            self.open_order.remove(index);
            self.open_order.push(tool);
        }
    }

    /// 按最新配置重新归属面板并丢弃历史 Context 槽；位置冲突保留最近打开项。
    ///
    /// @author kongweiguang
    pub(super) fn reproject(&mut self, settings: &ToolRailSettings) -> ToolRailPanelSlots {
        let mut slots = ToolRailPanelSlots::default();
        let mut normalized = Vec::with_capacity(self.open_order.len());
        for tool in self.open_order.iter().copied() {
            if tool == ToolRailToolId::Context {
                continue;
            }
            let slot = panel_slot_mut(&mut slots, panel_placement(settings, tool));
            if let Some(previous) = slot.replace(tool) {
                normalized.retain(|candidate| *candidate != previous);
            }
            normalized.retain(|candidate| *candidate != tool);
            normalized.push(tool);
        }
        self.open_order = normalized;
        slots
    }

    /// 按当前设置投影可见入口；历史 Context 标识不参与入口与宿主。
    /// 过滤后没有入口时仅临时露出 Agent 启动器，不回写用户的隐藏偏好。
    ///
    /// @author kongweiguang
    pub(super) fn project(&mut self, settings: &ToolRailSettings) -> ToolRailLayout {
        let panels = self.reproject(settings);
        let open = &self.open_order;
        let mut entries = ToolRailEntries::default();
        for tool in settings.order.iter().copied() {
            if tool == ToolRailToolId::Context {
                continue;
            }
            if settings.hidden.contains(&tool) && !open.contains(&tool) {
                continue;
            }
            if settings.bottom.contains(&tool) {
                entries.bottom.push(tool);
            } else {
                entries.main.push(tool);
            }
        }
        if entries.main.is_empty() && entries.bottom.is_empty() {
            // 空入口视图会使用户无法发现面板；临时投影固定工具，不修订配置所有权。
            entries.main.push(ToolRailToolId::AgentLauncher);
        }
        ToolRailLayout { entries, panels }
    }

    /// 记录所有右栏工具共用的宽度；回到统一默认值时移除覆盖，保持切换时几何稳定。
    ///
    /// @author kongweiguang
    pub(super) fn remember_attached_width(
        &mut self,
        tool: ToolRailToolId,
        measured: gpui_kit::Pixels,
        default: gpui_kit::Pixels,
    ) {
        let width = measured.as_f32();
        let minimum = workbench_layout::ATTACHED_DOCK_CONTENT_MIN_WIDTH + RESIZABLE_EDGE_GUTTER;
        let maximum = ATTACHED_DOCK_MAX_WIDTH + RESIZABLE_EDGE_GUTTER;
        if !width.is_finite() || !(minimum..=maximum).contains(&width) {
            return;
        }

        if tool == ToolRailToolId::Context {
            return;
        }
        if (width - default.as_f32()).abs() <= 0.5 {
            self.shared_attached_width = None;
        } else {
            self.shared_attached_width = Some(measured);
        }
    }

    /// 旧配置若包含不同工具宽度，沿用当前宿主值或最大值作为共享尺寸，并忽略已退役 Context 键。
    ///
    /// @author kongweiguang
    pub(super) fn restore_attached_widths(&mut self, widths: &BTreeMap<ToolRailToolId, u16>) {
        self.shared_attached_width = self
            .attached_tool
            .filter(|tool| *tool != ToolRailToolId::Context)
            .and_then(|tool| widths.get(&tool).copied())
            .or_else(|| {
                widths
                    .iter()
                    .filter(|(tool, _)| **tool != ToolRailToolId::Context)
                    .map(|(_, width)| *width)
                    .max()
            })
            .map(|width| px(f32::from(width) + RESIZABLE_EDGE_GUTTER));
    }

    /// 返回所有右栏工具共用的用户宽度；没有覆盖时由统一默认值决定。
    ///
    /// @author kongweiguang
    pub(super) fn attached_width(&self, tool: ToolRailToolId) -> Option<gpui_kit::Pixels> {
        (tool != ToolRailToolId::Context)
            .then_some(self.shared_attached_width)
            .flatten()
    }

    /// 返回最后一次由 Root 正式同步的右侧工具，用于设置重排前捕获仍在屏幕上的旧面板。
    ///
    /// @author kongweiguang
    pub(super) fn attached_tool(&self) -> Option<ToolRailToolId> {
        self.attached_tool
    }

    /// 只将当前停靠工具收到的明确 resize 记录为宽度，拒绝过期宿主的迟到回调。
    ///
    /// @author kongweiguang
    pub(super) fn remember_user_resize(
        &mut self,
        tool: ToolRailToolId,
        width: gpui_kit::Pixels,
    ) -> bool {
        if self.attached_tool != Some(tool) {
            return false;
        }
        let Some(default) = self.attached_default_width else {
            return false;
        };
        self.remember_attached_width(tool, width, default);
        true
    }

    /// 同步共享尺寸基准；已有用户覆盖时不被生命周期或目标变化覆盖。
    /// 即使 Kit 保留挂载尺寸，默认基准变化也只请求一次明确回填。
    ///
    /// @author kongweiguang
    pub(super) fn update_attached_default_width(
        &mut self,
        tool: ToolRailToolId,
        default_width: gpui_kit::Pixels,
    ) -> bool {
        if self.attached_tool != Some(tool) {
            return false;
        }

        let restore_default = self.attached_default_width != Some(default_width)
            && self.shared_attached_width.is_none();
        self.attached_default_width = Some(default_width);
        let width_is_now_default = self
            .shared_attached_width
            .is_some_and(|width| (width.as_f32() - default_width.as_f32()).abs() <= 0.5);
        if width_is_now_default {
            self.shared_attached_width = None;
        }
        restore_default
    }

    /// 切换或重排完成后同步工具及其默认宽度基准；普通 render 不更新以保留旧宿主身份。
    ///
    /// @author kongweiguang
    pub(super) fn set_attached_tool(
        &mut self,
        tool: Option<ToolRailToolId>,
        default_width: Option<gpui_kit::Pixels>,
    ) {
        self.attached_tool = tool;
        self.attached_default_width = default_width;
    }
}

impl RootView {
    /// Match main's drag-handle arrow controls using one rendered layout snapshot, keeping movement scoped to the floating tool.
    pub(super) fn handle_floating_tool_panel_key_down(
        &mut self,
        tool: ToolRailToolId,
        layout: FloatingPanelKeyLayout,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.to_ascii_lowercase();
        let (delta_x, delta_y) = match key.as_str() {
            "left" | "arrowleft" => (-1, 0),
            "right" | "arrowright" => (1, 0),
            "up" | "arrowup" => (0, -1),
            "down" | "arrowdown" => (0, 1),
            _ => return,
        };
        let settings = self.current_tool_rail_settings();
        if self.tool_rail_rendered_compact.0
            || self.tool_rail_state.project(&settings).panels.center != Some(tool)
        {
            return;
        }

        let step = if event.keystroke.modifiers.shift {
            40
        } else {
            12
        };
        window.prevent_default();
        cx.stop_propagation();
        if self.tool_rail_state.nudge_floating_position(
            tool,
            layout.rendered_position,
            (delta_x * step, delta_y * step),
            layout.viewport_width,
            layout.body_height,
            layout.geometry,
        ) {
            cx.notify();
        }
    }

    /// 单字段手势以末意图为基线，不能从响应式有效尺寸或 Agent 默认宽度重新拼整份布局。
    ///
    /// @author kongweiguang
    pub(super) fn current_workbench_layout_intent(&self) -> WorkbenchLayoutSettings {
        self.workbench_layout_intent
            .as_ref()
            .map(|intent| intent.layout.clone())
            .or_else(|| {
                self.settings_snapshot
                    .as_ref()
                    .map(|settings| settings.workbench_layout.clone())
            })
            .unwrap_or_default()
    }

    /// 显式手势/重试才创建新 request；SettingsLoaded、程序恢复与 watcher 从不调用此写入口。
    ///
    /// @author kongweiguang
    pub(super) fn request_workbench_layout_intent(
        &mut self,
        layout: WorkbenchLayoutSettings,
        cx: &mut Context<Self>,
    ) {
        let request_id = self.preferences_panel.update(cx, |panel, cx| {
            panel.request_workbench_layout(layout.clone(), cx)
        });
        self.workbench_layout_intent = Some(PendingWorkbenchLayout { request_id, layout });
        self.workbench_layout_save_error = None;
        cx.notify();
    }

    /// 加载/明确读回只装配首选尺寸；固定槽分配容器空间，恢复本身不产生用户 on_resize。
    ///
    /// @author kongweiguang
    pub(super) fn restore_workbench_layout(
        &mut self,
        layout: &WorkbenchLayoutSettings,
        cx: &mut Context<Self>,
    ) {
        self.host_sidebar_collapsed = layout.host_sidebar_collapsed;
        self.workbench_sidebar_restore_width = Some(px(
            f32::from(layout.host_sidebar_width) + RESIZABLE_EDGE_GUTTER
        ));
        self.workbench_sidebar_restore_pending = !self.host_sidebar_collapsed;
        self.workbench_bottom_restore_height = Some(px(f32::from(layout.bottom_panel_height)));
        self.workbench_bottom_restore_pending = true;
        self.tool_rail_state
            .restore_attached_widths(&layout.dock_panel_widths);
        self.prepare_attached_dock_width_restore(self.tool_rail_state.attached_tool());
        cx.notify();
    }

    /// 收据不恢复窗口尺寸；旧 A 只确认磁盘事实，新 B 的 RAM 投影与全局失败提示仍保留。
    /// 末收据只在 Running 继续；Stopping 的原 workspace CAS 必须自己回执，不能被布局成功抢占。
    ///
    /// @author kongweiguang
    pub(super) fn accept_workbench_layout_receipt(
        &mut self,
        request_id: u64,
        layout: WorkbenchLayoutSettings,
        cx: &mut Context<Self>,
    ) {
        if accept_workbench_layout_baseline(
            &mut self.settings_snapshot,
            &mut self.workbench_layout_intent,
            request_id,
            layout,
        ) {
            self.workbench_layout_save_error = None;
        }
        if super::quit::quit_preparation_is_running(
            self.host_lifecycle,
            self.host_shutdown_requested,
        ) && self.pending_quit_intent.is_some()
            && !self
                .preferences_panel
                .read(cx)
                .is_workbench_layout_save_in_flight()
        {
            let preferences = self.preferences_panel.clone();
            if preferences.update(cx, |panel, cx| panel.request_quit(cx)) {
                let _ = self.continue_quit_after_preferences(cx);
            }
        }
        cx.notify();
    }

    /// 侧栏只有用户完成拖动才写内容宽度；程序化恢复/容器调整不会经过该回调。
    ///
    /// @author kongweiguang
    fn record_user_sidebar_width(&mut self, width: gpui_kit::Pixels, cx: &mut Context<Self>) {
        let Some(content) = workbench_content_dimension(width, RESIZABLE_EDGE_GUTTER, 168, 360)
        else {
            return;
        };
        if self.host_sidebar_collapsed {
            return;
        }
        self.workbench_layout_restore_generation =
            self.workbench_layout_restore_generation.wrapping_add(1);
        self.workbench_sidebar_restore_width = Some(width);
        self.workbench_sidebar_restore_pending = false;
        let mut layout = self.current_workbench_layout_intent();
        layout.host_sidebar_width = content;
        self.request_workbench_layout_intent(layout, cx);
    }

    /// 底部没有侧向 resize 槽，直接保存逻辑高度；只在当前底部工具的拖动结束后受理。
    ///
    /// @author kongweiguang
    fn record_user_bottom_height(
        &mut self,
        tool: ToolRailToolId,
        height: gpui_kit::Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some(content) = workbench_content_dimension(height, 0.0, 180, 4_000) else {
            return;
        };
        let settings = self.current_tool_rail_settings();
        if self.tool_rail_state.project(&settings).panels.bottom != Some(tool) {
            return;
        }
        self.workbench_layout_restore_generation =
            self.workbench_layout_restore_generation.wrapping_add(1);
        self.workbench_bottom_restore_height = Some(height);
        self.workbench_bottom_restore_pending = false;
        let mut layout = self.current_workbench_layout_intent();
        layout.bottom_panel_height = content;
        self.request_workbench_layout_intent(layout, cx);
    }

    /// 显式核对只确认读回已存在且完全匹配的事实；不匹配继续保留错误与末意图，绝不重放。
    ///
    /// @author kongweiguang
    fn confirm_workbench_layout_readback(&mut self, cx: &mut Context<Self>) {
        if !self
            .preferences_panel
            .update(cx, |panel, cx| panel.confirm_workbench_layout_readback(cx))
        {
            self.workbench_layout_save_error = Some(crate::locale::text(
                "布局尚未核对：请在设置中重新读取；不一致时可明确重试当前布局。",
                "Layout is not verified. Reload in Settings; explicitly retry the current layout if it differs.",
            ));
            cx.notify();
        }
    }

    /// 响应式卸载只收起临时宿主，保留打开顺序与保存布局；显式关闭先经精确 owner 入口移除工具。
    /// 焦点仍只返回有效原控件，并把模态对话框和系统提示中的输入留给其当前所有者。
    ///
    /// @author kongweiguang
    pub(super) fn close_compact_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compact_host_drawer_open = false;
        self.compact_tool_drawer_open = false;
        let focus_is_blocked = logical_input_modal_blocked(
            self.preferences_modal_open,
            self.connections_modal_open,
            self.snippet_editor_modal_open || self.custom_agent_manager.is_some(),
            self.root_confirmation_owner.is_some(),
            self.pending_quit_intent.is_some(),
        ) || window.has_active_prompt();
        if !focus_is_blocked {
            if let Some(focus) = self.compact_drawer_return_focus.take() {
                if self.focus_handle.contains(&focus, window) {
                    window.focus(&focus, cx);
                } else {
                    window.focus(&self.focus_handle, cx);
                }
            } else {
                window.focus(&self.focus_handle, cx);
            }
        } else {
            self.compact_drawer_return_focus = None;
        }
        cx.notify();
    }

    /// 主机抽屉与工具抽屉共用焦点边界；从 Agent 抽屉切到主机抽屉时先同步撤销搜索定位。
    /// 返回位置仍保留最初的工作区焦点，不改变会话或 Agent 绑定。
    ///
    /// @author kongweiguang
    fn toggle_compact_host_drawer(
        &mut self,
        _event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_pending_pane_focus();
        self.pending_tool_rail_placement_focus = None;
        if self.compact_host_drawer_open {
            self.dismiss_compact_drawer(None, window, cx);
            return;
        }

        if self.compact_tool_drawer_open
            && self.tool_rail_rendered_compact.1 == Some(ToolRailToolId::AgentLauncher)
        {
            self.reconcile_agent_search_visibility(false, cx);
        }
        if !self.compact_host_drawer_open && !self.compact_tool_drawer_open {
            self.compact_drawer_return_focus = window.focused(cx);
        }
        self.compact_tool_drawer_open = false;
        self.compact_host_drawer_open = true;
        window.focus(&self.compact_drawer_focus_handle, cx);
        cx.notify();
    }

    /// 快捷键与图标共用激活入口；缓存投影只决定既有宿主的关闭，首次打开另按新事实复算同一预算。
    ///
    /// @author kongweiguang
    pub(super) fn activate_tool_rail_panel_from_shortcut(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (compact_panels, compact_active_tool) = self.tool_rail_rendered_compact;
        self.activate_compact_tool_rail_panel(
            tool,
            compact_active_tool,
            compact_panels,
            window,
            cx,
        );
    }

    /// 窄布局显式激活先撤销旧导航焦点意图；切离 Agent 抽屉时同步取消搜索提交门，避免等待下一次布局渲染。
    /// 首次打开可能使原宽布局转为 compact，必须按打开后的同一绘制预算挂载抽屉；关闭不重新打开。
    ///
    /// @author kongweiguang
    fn activate_compact_tool_rail_panel(
        &mut self,
        tool: ToolRailToolId,
        compact_active_tool: Option<ToolRailToolId>,
        compact_panels: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_tool_rail_placement_focus = None;
        if !compact_panels {
            self.toggle_tool_rail_panel(tool, window, cx);
            let settings = self.current_tool_rail_settings();
            let panels = self.tool_rail_state.project(&settings).panels;
            let plan = project_workbench_viewport(self, panels, window, cx).plan;
            if let Some(tool) = compact_tool_panel_open_target(tool, panels, plan) {
                self.clear_pending_pane_focus();
                self.pending_tool_rail_placement_focus = None;
                self.show_compact_tool_panel(tool, window, cx);
            }
            return;
        }

        self.clear_pending_pane_focus();
        if self.compact_tool_drawer_open
            && compact_active_tool == Some(ToolRailToolId::AgentLauncher)
            && tool != ToolRailToolId::AgentLauncher
        {
            self.reconcile_agent_search_visibility(false, cx);
        }
        if compact_tool_rail_panel_should_close(
            self.compact_tool_drawer_open,
            compact_active_tool,
            tool,
        ) {
            self.dismiss_compact_drawer(Some(tool), window, cx);
            return;
        }

        self.show_compact_tool_panel(tool, window, cx);
    }

    /// 新工具按 main 的单面板 compact 语义替换既有打开项；已打开工具只切换抽屉焦点。
    /// 关闭仍经共享切换路径完成，以同步 Agent 搜索、Dock 宽度与工具专属状态。
    ///
    /// @author kongweiguang
    fn show_compact_tool_panel(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tool_rail_state.open_order.contains(&tool) {
            let right_dock_was_collapsed = self.right_dock_collapsed;
            self.right_dock_collapsed = false;
            // Re-read the projected order after each close so a settings reproject cannot reopen a stale collision.
            while let Some(open_tool) = self.tool_rail_state.open_order.last().copied() {
                self.toggle_tool_rail_panel(open_tool, window, cx);
            }
            self.right_dock_collapsed = right_dock_was_collapsed;
            self.toggle_tool_rail_panel(tool, window, cx);
            self.right_dock_collapsed = right_dock_was_collapsed;
        }
        self.tool_rail_state.activate(tool);
        if !self.compact_host_drawer_open && !self.compact_tool_drawer_open {
            self.compact_drawer_return_focus = window.focused(cx);
        }
        self.compact_host_drawer_open = false;
        self.compact_tool_drawer_open = true;
        window.focus(&self.compact_drawer_focus_handle, cx);
        cx.notify();
    }

    /// 只在验收构建中打开活动态投影；隐藏窗口不接收 OS 激活，生产状态仍以窗口焦点为准。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub fn enable_right_rail_acceptance_activity(&mut self, cx: &mut Context<Self>) {
        self.right_rail_acceptance_active_override = true;
        cx.notify();
    }

    /// compact 验收直接幂等挂载 drawer；常规布局清理旧工具后走生产入口并按新预算复算宿主。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub fn activate_right_rail_tool_for_acceptance(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (compact_panels, compact_active_tool) = self.tool_rail_rendered_compact;
        if compact_panels {
            self.show_compact_tool_panel(tool, window, cx);
            return;
        }

        let open_tools = self.tool_rail_state.open_order.clone();
        for open_tool in open_tools {
            self.toggle_tool_rail_panel(open_tool, window, cx);
        }
        self.activate_compact_tool_rail_panel(
            tool,
            compact_active_tool,
            compact_panels,
            window,
            cx,
        );
    }

    /// compact 模式只报告抽屉已打开且仍在实际投影中的工具，避免验收把 open_order 误当作可见面板。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub fn right_rail_acceptance_active_tool(&self) -> Option<ToolRailToolId> {
        if self.tool_rail_rendered_compact.0 {
            if !self.compact_tool_drawer_open {
                return None;
            }
            return self
                .tool_rail_rendered_compact
                .1
                .filter(|tool| self.tool_rail_state.open_order.contains(tool));
        }
        self.tool_rail_state.open_order.last().copied()
    }
}

/// 窄屏抽屉由当前 Tab 的活动工具决定；主机抽屉占用同一焦点宿主时优先显示。
///
/// @author kongweiguang
pub(super) fn compact_tool_drawer_should_be_visible(
    compact_panels: bool,
    active_tool: Option<ToolRailToolId>,
    host_drawer_open: bool,
) -> bool {
    compact_panels && active_tool.is_some() && !host_drawer_open
}

/// 同一紧凑工具的再次激活只关闭已有抽屉；隐藏抽屉或其它工具不能被旧活动槽误判为关闭请求。
///
/// @author kongweiguang
pub(super) fn compact_tool_rail_panel_should_close(
    drawer_open: bool,
    active_tool: Option<ToolRailToolId>,
    requested_tool: ToolRailToolId,
) -> bool {
    drawer_open && active_tool == Some(requested_tool)
}

/// 新增工具可能改变宽高预算，故只根据 toggle 后仍打开的原工具与新投影补齐宿主，不重放打开用例。
///
/// @author kongweiguang
pub(super) fn compact_tool_panel_open_target(
    requested_tool: ToolRailToolId,
    panels: ToolRailPanelSlots,
    plan: workbench_layout::WorkbenchLayoutPlan,
) -> Option<ToolRailToolId> {
    (plan.compact_panels && tool_is_in_panel_slots(requested_tool, panels))
        .then_some(requested_tool)
}

/// All right tools share main's total width after removing the rail while retaining the GPUI resize gutter.
///
/// @author kongweiguang
pub(super) fn attached_dock_default_width(viewport_width: f32) -> gpui_kit::Pixels {
    px(
        workbench_layout::main_attached_dock_total_to_gpui_panel_width(
            workbench_layout::initial_tool_panel_width(viewport_width),
        ),
    )
}

/// 文件与容器工具固定右侧；其余工具沿用配置，兼容停靠值只在投影时忽略而不改写磁盘。
///
/// @author kongweiguang
fn panel_placement(settings: &ToolRailSettings, tool: ToolRailToolId) -> ToolRailPanelPlacement {
    settings.effective_panel_placement(tool)
}

/// 展开后预算不足时立即显示同一目录抽屉；用户显式收起仍保持收起，不替用户补开面板。
///
/// @author kongweiguang
fn sidebar_expansion_needs_drawer(user_collapsed: bool, compact_width: bool) -> bool {
    !user_collapsed && compact_width
}

/// 只集中映射四种有限宿主，避免各个 Root 回调分散维护冲突规则。
///
/// @author kongweiguang
fn panel_slot_mut(
    slots: &mut ToolRailPanelSlots,
    placement: ToolRailPanelPlacement,
) -> &mut Option<ToolRailToolId> {
    match placement {
        ToolRailPanelPlacement::Attached => &mut slots.attached,
        ToolRailPanelPlacement::Left => &mut slots.left,
        ToolRailPanelPlacement::Bottom => &mut slots.bottom,
        ToolRailPanelPlacement::Center => &mut slots.center,
    }
}

/// 只把 Root 投影好的唯一 Entity 放进对应宿主；窗口变化固定侧栏与 Dock 基准宽度，由中心内容吸收余量。
/// 标题栏 Tab 列表和唯一的新建入口由 WorkspaceTabs 管理，窄窗由其列表自身滚动承接。
/// 壁纸材料仅用于工作台表面，设置/连接弹窗本体需不透明以隔离底层内容。
/// 通过 Kit 公开的 base 回调类型只调整 1px 线色，保留组件原有 9px 命中带与拖动实现。
/// 隐藏工具的显式回调把 Window 交给 Root 恢复正文焦点；居中浮层独占临时焦点，子控件先处理 Escape。
/// 截断诊断行保留独立且可预测的节点身份，避免完整提示丢失或错误复用。
/// 统一主机表单首选 960×720px 容纳分类与本地启动参数，窄矮窗按四边 16px 夹紧；正文滚动与固定操作行仍由表单持有。
/// 持久尺寸只由真实拖动结束回调提交，固定槽程序恢复与响应式夹紧不参与 settings 写入。
/// 折叠侧栏复用标题栏唯一展开入口，正文不保留窄轨；设置仍可从展开后的侧栏或快捷键进入。
/// 终端下方不保留常驻状态条，必要状态改用可关闭的浮层提示，不占用终端高度。
/// Agent 终端可见时 launcher 仅按头部与恢复/错误反馈定高，终端独占 Dock 的剩余高度；无终端时入口继续填满并居中。
/// 搜索按实际可见宿主挂起或恢复，状态位避免重绘反复重启防抖，恢复查询不获取焦点。
/// Center 浮窗独立于终端和 Rail，拖动坐标受 Shell 边界约束且不设遮罩或焦点陷阱。
/// 绘制与首次激活共用只读视口投影；主机栏展开也按展开后的预算打开抽屉，不能只挤走工具却不显示目录。
/// 设置弹层限制为可扫读的双栏宽高；密集分类继续内部滚动，固定操作栏与窄窗视口夹紧规则不变。
/// 两侧分隔槽平时透明，悬停光效只由 Kit 的拖动状态驱动；收起右栏仍补齐与左侧相同的留白。
/// 运维表单接收扣除边框的宿主高度，正文滚动不能挤出固定操作栏。
/// 右栏决策弹框按 main 先过滤背景再绘制主题玻璃表面；紧凑遮罩覆盖 body，面板单独内缩 8px。
/// 窄屏工具抽屉从当前 Tab 活动工具派生，常规停靠宿主只处理精确 owner 冒泡到达的未消费 Escape。
/// Root 确认、连接提示与 Kit 模态优先持有焦点；只暂停抽屉的焦点回收，保留 main 的模态背景。
/// 主机弹层统一按 rem 缩放并夹紧视口；主机树插槽允许收缩，使内部滚动区能接收剩余高度。
/// 工作台按 main 的局部色板呈现；标签区的唯一前导间距与抽屉按钮到分隔线的距离一致。
///
/// @author kongweiguang
pub(super) fn render_workbench(
    root: &mut RootView,
    host_navigation: Option<Entity<HostNavigation>>,
    center_content: AnyElement,
    agent_terminal_content: Option<AnyElement>,
    tool_panels: WorkbenchToolPanels,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> impl IntoElement + use<> {
    // 主机行只展示从真实 pane 生命周期汇总的状态，导航叶子按值去重通知。
    super::connections::refresh_host_session_projection(root, cx);
    let feedback_obscured = logical_input_modal_blocked(
        root.preferences_modal_open,
        root.connections_modal_open,
        root.snippet_editor_modal_open || root.custom_agent_manager.is_some(),
        root.root_confirmation_owner.is_some(),
        root.pending_quit_intent.is_some() || root.quit_ready_intent.is_some(),
    ) || root.connection_prompt_modal_open
        || root.split_target_picker.read(cx).is_open()
        || root
            .quick_open_view
            .as_ref()
            .is_some_and(|view| view.read(cx).is_open())
        || window.has_active_prompt();
    track_workbench_status_toast(root, feedback_obscured, cx);
    let theme = ThemeTokens::current(window, cx).for_workbench();
    let resize_handle_appearance = super::workbench_resize::appearance(theme.accent);
    let ui_font_family =
        cx.read_global::<gpui_kit::component::Theme, _>(|theme, _| theme.font_family.clone());
    // 根层保持透明，让 Root 绘制在最底部的 root_base 与品牌背景图继续透出。
    let sidebar_background = theme.materials.chrome;
    let workspace_background = theme.materials.workspace;
    let panel_background = theme.materials.panel;
    let terminal_header_background = theme.materials.terminal_header;
    let show_empty_terminal =
        root.center_panel == CenterPanel::Terminal && !workspace_has_active_tab(root);
    let WorkbenchToolPanels {
        layout,
        mut renderers,
    } = tool_panels;
    let snippet_editor_open = super::snippet_editor::is_open(root);
    if snippet_editor_open {
        // Center Commands 与 Snippets ToolRail 原来共用 Entity；模态期间只让根层挂载它一次。
        renderers.snippets = super::snippet_editor::suspended_renderer();
    }
    let center_content = if snippet_editor_open && root.center_panel == CenterPanel::Commands {
        super::snippet_editor::suspended_renderer()
    } else {
        center_content
    };
    let ToolRailLayout { entries, panels } = layout;

    let focused_session = (root.center_panel == CenterPanel::Terminal)
        .then(|| {
            root.workspace_controller
                .as_ref()
                .and_then(|controller| focused_workspace_pane(controller.snapshot()))
                .and_then(|pane_id| root.workspace_runtime.session_for_pane(&pane_id).cloned())
        })
        .flatten();
    if let Some((session_id, tools)) = focused_session.as_ref().and_then(|session_id| {
        root.workspace_tools
            .get(session_id)
            .map(|tools| (session_id, tools))
    }) {
        let session_id = session_id.clone();
        tools.update(cx, |tools, cx| {
            tools.set_session(Some(session_id), window, cx);
        });
    }

    let viewport = window.viewport_size();
    root.tool_rail_state
        .remember_viewport_width(viewport.width.as_f32());
    let WorkbenchViewportProjection {
        plan: layout_plan,
        failed_file_tab,
        workspace_notice,
        recovery_max_height,
    } = project_workbench_viewport(root, panels, window, cx);
    if (root.compact_host_drawer_open && !layout_plan.compact_width)
        || (root.compact_tool_drawer_open && !layout_plan.compact_panels)
    {
        // 视口恢复到正常布局后卸载抽屉焦点节点，并回到打开前的有效控件。
        root.close_compact_drawer(window, cx);
    }
    root.tool_rail_rendered_compact = (layout_plan.compact_panels, layout_plan.compact_active_tool);
    let effective_host_sidebar_collapsed = layout_plan.effective_host_sidebar_collapsed;
    let compact_panels = layout_plan.compact_panels;
    let compact_active_tool = layout_plan.compact_active_tool;
    let compact_drawer_blocked = root.custom_agent_manager.is_some()
        || root.preferences_modal_open
        || root.connections_modal_open
        || snippet_editor_open
        || root.pending_quit_intent.is_some()
        || window.has_active_prompt();
    let compact_drawer_focus_blocked = compact_drawer_blocked
        || root.connection_prompt_modal_open
        || root.root_confirmation_owner.is_some()
        || window.has_active_dialog(cx);
    let should_show_active_compact_tool = compact_tool_drawer_should_be_visible(
        compact_panels,
        compact_active_tool,
        root.compact_host_drawer_open,
    );
    // 显隐由活动工具投影驱动；只在焦点离开抽屉时回收，保留最初的返回焦点。
    if !compact_drawer_focus_blocked {
        if should_show_active_compact_tool {
            if !root.compact_tool_drawer_open {
                if root.compact_drawer_return_focus.is_none() {
                    root.compact_drawer_return_focus = window.focused(cx);
                }
                root.compact_tool_drawer_open = true;
                window.focus(&root.compact_drawer_focus_handle, cx);
            } else if !root
                .compact_drawer_focus_handle
                .contains_focused(window, cx)
            {
                window.focus(&root.compact_drawer_focus_handle, cx);
            }
        } else if root.compact_tool_drawer_open && !root.compact_host_drawer_open {
            root.close_compact_drawer(window, cx);
        }
    }
    let show_compact_host_drawer =
        layout_plan.compact_width && root.compact_host_drawer_open && !compact_drawer_blocked;
    let show_compact_tool_drawer =
        should_show_active_compact_tool && root.compact_tool_drawer_open && !compact_drawer_blocked;

    let title_brand = div()
        .id("workbench-title-brand")
        .occlude()
        .w(px(HOST_NAVIGATION_CONTROL_WIDTH))
        .flex_shrink_0()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .relative()
        .child(
            div()
                .absolute()
                .right_0()
                .top_0()
                .h_full()
                .flex()
                .items_center()
                .child(crate::features::navigation_chrome::navigation_separator(
                    theme.border,
                )),
        );
    let sidebar_toggle_label = if layout_plan.compact_width {
        if root.compact_host_drawer_open {
            crate::locale::text("关闭主机列表", "Close host list")
        } else {
            crate::locale::text("显示主机列表", "Show host list")
        }
    } else if root.host_sidebar_collapsed {
        crate::locale::text("展开主机侧栏", "Show host sidebar")
    } else {
        crate::locale::text("收起主机侧栏", "Hide host sidebar")
    };
    let title_brand = title_brand.child(
        Button::new("workbench-toggle-host-sidebar")
            .compact()
            .ghost()
            .icon(IconName::PanelLeft)
            .accessibility_label(sidebar_toggle_label.clone())
            .tooltip(sidebar_toggle_label.clone())
            .tab_stop(true)
            .on_click(cx.listener(move |root, event, window, cx| {
                if layout_plan.compact_width {
                    root.toggle_compact_host_drawer(event, window, cx);
                } else {
                    root.toggle_host_sidebar(event, window, cx);
                    let settings = root.current_tool_rail_settings();
                    let panels = root.tool_rail_state.project(&settings).panels;
                    let projected = project_workbench_viewport(root, panels, window, cx);
                    if sidebar_expansion_needs_drawer(
                        root.host_sidebar_collapsed,
                        projected.plan.compact_width,
                    ) {
                        root.toggle_compact_host_drawer(event, window, cx);
                    }
                }
            })),
    );
    let title_content = div()
        .id("workbench-title-content")
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .flex()
        .items_center()
        .child(title_brand)
        .child(
            div()
                .id("workbench-tabs-bar")
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .flex()
                .items_center()
                .pl_1()
                .gap_1()
                .overflow_hidden()
                .child(
                    div()
                        .id("workbench-tabs-list")
                        // WorkspaceTabs 始终持有唯一的新建入口，含空态与溢出菜单布局。
                        .flex_1()
                        .flex_shrink_1()
                        .min_w(px(0.))
                        .h_full()
                        .overflow_hidden()
                        .child(root.workspace_tabs_view.clone()),
                ),
        );
    let title_content = title_content.into_any_element();

    #[cfg(target_os = "windows")]
    let title_bar = windows_workbench_title_bar(title_content, sidebar_background, theme, window);
    #[cfg(not(target_os = "windows"))]
    let title_bar = {
        let root_entity = cx.entity();
        TitleBar::new()
            .h(px(36.))
            .bg(sidebar_background)
            .border_b_1()
            .border_color(theme.border)
            .on_close_window(move |_: &ClickEvent, window, app| {
                root_entity.update(app, |root, cx| {
                    root.handle_window_close_request(window, cx);
                });
            })
            .child(title_content)
    };

    let compact_host_navigation = host_navigation.clone();
    let (footer_x, footer_y) = match super::preferences::current_interface_density(root) {
        InterfaceDensity::Compact => (12., 3.),
        InterfaceDensity::Comfortable => (14., 3.),
        InterfaceDensity::Spacious => (16., 5.),
    };
    let left_sidebar = {
        let mut sidebar = div()
            .id("workbench-left-sidebar")
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(sidebar_background)
            .text_color(theme.foreground)
            .child(host_navigation.map_or_else(
                || {
                    div()
                        .id("workbench-host-navigation-unavailable")
                        .flex_1()
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "主机导航尚未配置。",
                            "Host navigation is not configured.",
                        ))
                },
                |navigation| {
                    div()
                        .id("workbench-host-navigation-slot")
                        .flex_1()
                        .min_h(px(0.))
                        .child(navigation)
                },
            ));
        sidebar = sidebar.child(
            div()
                .id("workbench-sidebar-footer")
                .w_full()
                .relative()
                .flex_shrink_0()
                .px(px(footer_x))
                .py(px(footer_y))
                .flex()
                .items_center()
                .gap_1()
                .bg(sidebar_background)
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left(px(footer_x + 8.))
                        .right(px(footer_x + 8.))
                        .h(px(1.))
                        .bg(theme.border),
                )
                .child(render_settings_button(
                    "workbench-open-preferences-sidebar",
                    "workbench-open-preferences-sidebar-tooltip",
                    cx.listener(RootView::open_preferences),
                ))
                .child(div().flex_1())
                .child(
                    Button::new("workbench-create-host")
                        .compact()
                        .ghost()
                        .icon(IconName::Plus)
                        .accessibility_label(crate::locale::text("新建主机", "Create host"))
                        .tooltip(crate::locale::text("新建主机", "Create host"))
                        .tab_stop(true)
                        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
                            if let Some(panel) = root.connections_panel.clone() {
                                if root.prepare_connection_editor_modal(window, cx) {
                                    panel.update(cx, |panel, cx| {
                                        panel.begin_create_host(window, cx)
                                    });
                                }
                            } else {
                                root.report_host_status("主机连接服务尚未配置。", cx);
                            }
                        })),
                ),
        );
        sidebar.into_any_element()
    };

    let left_tool_panel = (!compact_panels)
        .then_some(panels.left)
        .flatten()
        .map(|tool| {
            let content = renderers.take_for(tool);
            div()
                .id("workbench-tool-panel-left-host")
                .on_key_down(cx.listener(move |root, event, window, cx| {
                    root.handle_docked_tool_panel_key_down(
                        ToolRailPanelPlacement::Left,
                        tool,
                        event,
                        window,
                        cx,
                    );
                }))
                .h_full()
                .min_w(px(0.))
                .child(workbench_tool_panel_slot(
                    "workbench-tool-panel-left",
                    content,
                ))
        });

    let mut center_column = div()
        .id("workbench-center-column")
        .flex_1()
        .min_w(px(layout_plan.center_min_width))
        .h_full()
        .flex()
        .flex_col()
        .overflow_hidden();

    if root.configuration_recovery.mode
        == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery
    {
        let recovery_rows = root
            .configuration_recovery
            .issues
            .iter()
            .enumerate()
            .map(|(index, issue)| {
                let path = super::safe_relative_configuration_path(&issue.relative_path);
                let category = match issue.kind {
                    kerminal_domain::preferences::ConfigurationIssueKind::MalformedToml => {
                        crate::locale::text("TOML 格式错误", "Invalid TOML")
                    }
                    kerminal_domain::preferences::ConfigurationIssueKind::UnsupportedSchema => {
                        crate::locale::text("配置版本不受支持", "Unsupported configuration version")
                    }
                    kerminal_domain::preferences::ConfigurationIssueKind::Unreadable => {
                        crate::locale::text("配置无法读取", "Configuration could not be read")
                    }
                };
                let location = match (issue.line, issue.column) {
                    (Some(line), Some(column)) => crate::locale::text(
                        format!("第 {line} 行，第 {column} 列"),
                        format!("Line {line}, column {column}"),
                    ),
                    (Some(line), None) => {
                        crate::locale::text(format!("第 {line} 行"), format!("Line {line}"))
                    }
                    (None, Some(column)) => {
                        crate::locale::text(format!("第 {column} 列"), format!("Column {column}"))
                    }
                    (None, None) => crate::locale::text("位置未提供", "Location unavailable"),
                };
                let diagnostic = format!("{path} · {category} · {location}");
                let diagnostic_tooltip = diagnostic.clone();
                div()
                    .id(format!(
                        "workbench-configuration-recovery-diagnostic-{index}-{path}"
                    ))
                    .h(px(super::RECOVERY_ISSUE_ROW_HEIGHT))
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .tooltip(move |window, cx| {
                        Tooltip::new(diagnostic_tooltip.clone()).build(window, cx)
                    })
                    .child(diagnostic)
            })
            .collect::<Vec<_>>();
        let recovery_title = crate::locale::text(
            format!(
                "配置只读恢复模式：配置写入已停用，共 {} 项诊断。",
                root.configuration_recovery.issues.len()
            ),
            format!(
                "Configuration is in read-only recovery mode: writes are disabled; {} diagnostics found.",
                root.configuration_recovery.issues.len()
            ),
        );
        let recovery_title_tooltip = recovery_title.clone();
        center_column = center_column.child(
            div()
                .id("workbench-configuration-recovery")
                .w_full()
                .max_h(px(recovery_max_height))
                .flex()
                .flex_col()
                .px_3()
                .overflow_hidden()
                .bg(panel_background)
                .child(
                    div()
                        .id("workbench-configuration-recovery-header")
                        .w_full()
                        .h(px(super::RECOVERY_BANNER_HEADER_HEIGHT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .id("workbench-configuration-recovery-title")
                                .w_full()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(12.))
                                .text_color(theme.error)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(recovery_title_tooltip.clone()).build(window, cx)
                                })
                                .child(recovery_title),
                        ),
                )
                .child(
                    div()
                        .id("workbench-configuration-recovery-rows")
                        .w_full()
                        .max_h(px(
                            recovery_max_height - super::RECOVERY_BANNER_HEADER_HEIGHT
                        ))
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .children(recovery_rows),
                ),
        );
    }

    if let Some(notice) = workspace_notice {
        let notice_tooltip = notice.clone();
        center_column = center_column.child(
            div()
                .id("workbench-workspace-notice")
                .w_full()
                .min_w(px(0.))
                .px_3()
                .py_2()
                .overflow_hidden()
                .bg(panel_background)
                .text_size(px(12.))
                .text_color(theme.muted)
                .whitespace_nowrap()
                .text_ellipsis()
                .tooltip(move |window, cx| Tooltip::new(notice_tooltip.clone()).build(window, cx))
                .child(notice),
        );
    }

    // 文件恢复错误属于原 tab；重试仍只经过 Root 的 Failed-only 入口，Pending 时不会重复提交。
    if let Some((tab_id, message)) = failed_file_tab {
        let retry_label = crate::locale::text("重试恢复", "Retry restore");
        let message_tooltip = message.clone();
        let retry = Button::new("workbench-retry-file-tab-restore")
            .compact()
            .secondary()
            .label(retry_label.clone())
            .accessibility_label(retry_label.clone())
            .tooltip(retry_label)
            .tab_stop(true)
            .on_click(cx.listener(move |root, _, window, cx| {
                root.retry_workspace_file_tab(&tab_id, window, cx);
            }));
        center_column = center_column.child(
            div()
                .id("workbench-file-tab-restore-failure")
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .px_3()
                .py_2()
                .bg(panel_background)
                .text_size(px(12.))
                .text_color(theme.error)
                .child(
                    div()
                        .id("workbench-file-tab-restore-failure-message")
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .tooltip(move |window, cx| {
                            Tooltip::new(message_tooltip.clone()).build(window, cx)
                        })
                        .child(message),
                )
                .child(div().flex_shrink_0().child(retry)),
        );
    }

    let center_content_slot = div()
        .id("workbench-center-content")
        .flex_1()
        .min_h(px(0.))
        .overflow_hidden()
        .child(
            div()
                .id("workbench-center-surface")
                .size_full()
                .overflow_hidden()
                .bg(workspace_background)
                .child(if show_empty_terminal {
                    empty_terminal_workspace_slot(
                        theme,
                        root.is_creating_session,
                        root.connections_panel.is_some(),
                        cx.listener(RootView::create_local_terminal),
                        cx.listener(|root, _: &ClickEvent, window, cx| {
                            let Some(panel) = root.connections_panel.clone() else {
                                root.report_host_status("主机连接服务尚未配置。", cx);
                                return;
                            };
                            if root.prepare_connection_editor_modal(window, cx) {
                                panel.update(cx, |panel, cx| panel.begin_create_host(window, cx));
                            }
                        }),
                    )
                } else {
                    center_content
                }),
        )
        .into_any_element();

    if let Some(tool) = panels.bottom {
        if compact_panels {
            center_column = center_column.child(center_content_slot);
        } else {
            let content = renderers.take_for(tool);
            let bottom_panel = div()
                .id("workbench-tool-panel-bottom-host")
                .on_key_down(cx.listener(move |root, event, window, cx| {
                    root.handle_docked_tool_panel_key_down(
                        ToolRailPanelPlacement::Bottom,
                        tool,
                        event,
                        window,
                        cx,
                    );
                }))
                .size_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(workbench_tool_panel_slot(
                    "workbench-tool-panel-bottom",
                    content,
                ));
            let max_height = layout_plan
                .bottom_panel_max_height
                .max(workbench_layout::BOTTOM_TOOL_MIN_HEIGHT);
            let preferred_height = root
                .workbench_bottom_resizable_state
                .read(cx)
                .preferred_panel_size(1)
                .or_else(|| {
                    root.workbench_bottom_resizable_state
                        .read(cx)
                        .sizes()
                        .get(1)
                        .copied()
                })
                .unwrap_or(px(240.))
                .as_f32()
                .clamp(workbench_layout::BOTTOM_TOOL_MIN_HEIGHT, max_height);
            let stack = v_resizable("workbench-bottom-tool-split")
                .with_state(&root.workbench_bottom_resizable_state)
                .preserve_panel_size_on_container_resize(1)
                .with_handle_appearance(resize_handle_appearance.clone())
                .on_resize({
                    let root_entity = cx.entity();
                    move |state, _, app| {
                        let height = state
                            .read(app)
                            .preferred_panel_size(1)
                            .or_else(|| state.read(app).sizes().get(1).copied());
                        if let Some(height) = height {
                            root_entity.update(app, |root, cx| {
                                root.record_user_bottom_height(tool, height, cx);
                            });
                        }
                    }
                })
                .child(
                    resizable_panel()
                        .size_range(px(workbench_layout::CENTER_MIN_HEIGHT)..px(4_000.))
                        .child(center_content_slot),
                )
                .child(
                    resizable_panel()
                        .size(px(preferred_height))
                        // 固定范围保留大高度首选；容器策略临时让出中心最小高度，而不截断持久目标。
                        .size_range(px(workbench_layout::BOTTOM_TOOL_MIN_HEIGHT)..px(4_000.))
                        .flex_none()
                        .child(bottom_panel),
                );
            center_column = center_column.child(
                div()
                    .id("workbench-bottom-resizable-stack")
                    .flex_1()
                    .min_h(px(workbench_layout::CENTER_MIN_HEIGHT))
                    .child(stack),
            );
        }
    } else {
        center_column = center_column.child(center_content_slot);
    }

    let density = super::preferences::interface_density_metrics(root);
    let rail_button_height =
        tool_rail_button_height(super::preferences::current_interface_density(root));
    let mut rail_main = div()
        .id("workbench-tool-rail-main")
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .gap(px(density.item_gap / 2.0))
        .items_center()
        .overflow_y_scroll();
    for tool in entries.main.iter().copied() {
        let selected = if compact_panels {
            show_compact_tool_drawer && compact_active_tool == Some(tool)
        } else {
            tool_is_in_panel_slots(tool, panels)
        };
        let toggled = if compact_panels {
            root.tool_rail_state.open_order.contains(&tool)
        } else {
            selected
        };
        let button = workbench_rail_button(
            tool_rail_button_id(tool),
            tool_rail_icon(tool),
            tool_rail_label(tool),
            rail_button_height,
            selected,
            Some(toggled),
            cx.listener(move |root, _: &ClickEvent, window, cx| {
                root.activate_compact_tool_rail_panel(
                    tool,
                    compact_active_tool,
                    compact_panels,
                    window,
                    cx,
                );
            }),
        );
        rail_main = rail_main.child(button.custom(theme.rail_button_variant(selected, cx)));
    }
    let mut rail_bottom = div()
        .id("workbench-tool-rail-bottom")
        .w_full()
        .flex_shrink_0()
        .min_h(tool_rail_partition_min_height(
            &entries.bottom,
            rail_button_height,
        ))
        .max_h(relative(0.55))
        .flex()
        .flex_col()
        .gap(px(density.item_gap / 2.0))
        .items_center()
        .pt(px(density.item_gap / 2.0))
        .overflow_y_scroll()
        .when(!entries.main.is_empty(), |this| {
            this.border_t_1().border_color(theme.tool_panel_border)
        });
    for tool in entries.bottom.iter().copied() {
        let selected = if compact_panels {
            show_compact_tool_drawer && compact_active_tool == Some(tool)
        } else {
            tool_is_in_panel_slots(tool, panels)
        };
        let toggled = if compact_panels {
            root.tool_rail_state.open_order.contains(&tool)
        } else {
            selected
        };
        let button = workbench_rail_button(
            tool_rail_button_id(tool),
            tool_rail_icon(tool),
            tool_rail_label(tool),
            rail_button_height,
            selected,
            Some(toggled),
            cx.listener(move |root, _: &ClickEvent, window, cx| {
                root.activate_compact_tool_rail_panel(
                    tool,
                    compact_active_tool,
                    compact_panels,
                    window,
                    cx,
                );
            }),
        );
        rail_bottom = rail_bottom.child(button.custom(theme.rail_button_variant(selected, cx)));
    }
    let tool_rail = div()
        .id("workbench-tool-rail")
        .debug_selector(|| "workbench-tool-rail".into())
        .w(px(TOOL_RAIL_WIDTH))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .py(px(density.tool_rail_padding_y))
        .border_l_1()
        .border_color(theme.tool_panel_border)
        .overflow_hidden()
        .bg(sidebar_background)
        .text_color(theme.muted)
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|root, _, window, cx| {
                window.prevent_default();
                root.open_tool_rail_customizer(window, cx);
                cx.stop_propagation();
            }),
        )
        .on_key_down(cx.listener(|root, event: &KeyDownEvent, window, cx| {
            let opens_menu = event.keystroke.key == "ContextMenu"
                || (event.keystroke.key.eq_ignore_ascii_case("F10")
                    && event.keystroke.modifiers.shift);
            if opens_menu && !event.is_held {
                window.prevent_default();
                root.open_tool_rail_customizer(window, cx);
                cx.stop_propagation();
            }
        }))
        .child(rail_main)
        .child(rail_bottom);
    let has_agent_terminal = agent_terminal_content.is_some();
    let show_agent_terminal =
        has_agent_terminal && panels.attached == Some(ToolRailToolId::AgentLauncher);
    let mut agent_dock = div()
        .id("workbench-agent-dock")
        .on_key_down(cx.listener({
            let rendered_tool = panels.attached.unwrap_or(ToolRailToolId::AgentLauncher);
            move |root, event, window, cx| {
                root.handle_docked_tool_panel_key_down(
                    ToolRailPanelPlacement::Attached,
                    rendered_tool,
                    event,
                    window,
                    cx,
                );
            }
        }))
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.tool_panel_surface);
    if let Some(tool) = panels.attached {
        let content = renderers.take_for(tool);
        let panel_slot = if tool == ToolRailToolId::AgentLauncher {
            let mut launcher = div()
                .id("workbench-agent-launcher-panel-host")
                .w_full()
                .flex()
                .flex_col()
                .overflow_hidden();
            if show_agent_terminal {
                // 运行态 chrome 不能与 Canvas 平分 Dock；保留恢复与错误反馈的内容高度，其余交给终端。
                launcher = launcher.min_h(px(40.)).overflow_y_scroll();
            } else {
                launcher = launcher.flex_1().min_h(px(0.));
            }
            launcher.child(content).into_any_element()
        } else {
            workbench_tool_panel_slot("workbench-tool-panel-attached", content).into_any_element()
        };
        agent_dock = agent_dock.child(panel_slot);
    }
    if let Some(agent_terminal) = agent_terminal_content.filter(|_| show_agent_terminal) {
        agent_dock = agent_dock.child(
            div()
                .id("workbench-agent-terminal-slot")
                .w_full()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(terminal_header_background)
                .child(agent_terminal),
        );
    }
    let mut agent_dock_element = Some(agent_dock.into_any_element());

    let left_default_width = px(workbench_layout::initial_tool_panel_width(
        viewport.width.as_f32(),
    ) + RESIZABLE_EDGE_GUTTER);
    let initial_left_tool_width = root
        .workbench_left_tool_resizable_state
        .read(cx)
        .preferred_panel_size(0)
        .or_else(|| {
            root.workbench_left_tool_resizable_state
                .read(cx)
                .sizes()
                .first()
                .copied()
        })
        .unwrap_or(left_default_width);
    let center_area = if let Some(left_panel) = left_tool_panel {
        h_resizable("workbench-left-tool-panel-split")
            .preserve_panel_size_on_container_resize(0)
            .with_state(&root.workbench_left_tool_resizable_state)
            .with_handle_appearance(resize_handle_appearance.clone())
            .child(
                resizable_panel()
                    .size(initial_left_tool_width)
                    .size_range(
                        px(LEFT_TOOL_PANEL_MIN_WIDTH + RESIZABLE_EDGE_GUTTER)
                            ..px(layout_plan.left_tool_panel_max_width + RESIZABLE_EDGE_GUTTER),
                    )
                    .flex_none()
                    .child(
                        div()
                            .size_full()
                            .pr(px(RESIZABLE_EDGE_GUTTER))
                            .child(left_panel),
                    ),
            )
            .child(
                resizable_panel()
                    .size_range(px(layout_plan.center_min_width)..px(4_000.0))
                    .child(center_column),
            )
            .into_any_element()
    } else {
        center_column.into_any_element()
    };

    let dock_is_visible = !compact_panels
        && !root.right_dock_collapsed
        && (panels.attached.is_some() || show_agent_terminal);
    root.reconcile_workspace_search_visibility(cx);
    root.reconcile_agent_search_visibility(
        show_agent_terminal
            && (dock_is_visible
                || show_compact_tool_drawer
                    && compact_active_tool == Some(ToolRailToolId::AgentLauncher)),
        cx,
    );
    let dock_tool = panels.attached.unwrap_or(ToolRailToolId::AgentLauncher);
    let dock_default_width = attached_dock_default_width(viewport.width.as_f32());
    if root
        .tool_rail_state
        .update_attached_default_width(dock_tool, dock_default_width)
    {
        root.prepare_attached_dock_width_restore(Some(dock_tool));
    }
    let initial_dock_width = root
        .tool_rail_state
        .attached_width(dock_tool)
        .unwrap_or(dock_default_width);
    let restore_sidebar_now =
        root.workbench_sidebar_restore_pending && !effective_host_sidebar_collapsed;
    let restore_dock_now = root.workbench_dock_restore_pending && dock_is_visible;
    let restore_bottom_now =
        root.workbench_bottom_restore_pending && !compact_panels && panels.bottom.is_some();
    if restore_sidebar_now || restore_dock_now || restore_bottom_now {
        let generation = root.workbench_layout_restore_generation;
        let sidebar_width = root.workbench_sidebar_restore_width;
        let dock_width = root.workbench_dock_restore_width;
        let bottom_height = root.workbench_bottom_restore_height;
        let bottom_tool = panels.bottom;
        // 到匹配 generation 的真实回填后才清 pending；其它尺寸手势可能撤销当前帧，
        // 但不能连同尚未执行的侧栏/底部持久目标一起丢失。
        // 容器尺寸变化由固定槽策略自行保持像素宽度。
        let root_entity = cx.entity();
        window.on_next_frame(move |window, app| {
            let mut restore_dock_after_sidebar = false;
            root_entity.update(app, |root, cx| {
                if root.workbench_layout_restore_generation != generation {
                    return;
                }
                if restore_sidebar_now
                    && !root.host_sidebar_collapsed
                    && let Some(width) = sidebar_width
                {
                    let restored =
                        root.workbench_sidebar_resizable_state
                            .update(cx, |state, cx| {
                                if state.sizes().len() < 2 {
                                    return false;
                                }
                                state.resize_panel(0, width, window, cx);
                                true
                            });
                    root.workbench_sidebar_restore_pending = !restored;
                }
                if restore_bottom_now && let Some(height) = bottom_height {
                    let settings = root.current_tool_rail_settings();
                    if root.tool_rail_state.project(&settings).panels.bottom == bottom_tool {
                        let restored =
                            root.workbench_bottom_resizable_state
                                .update(cx, |state, cx| {
                                    if state.sizes().len() < 2 {
                                        return false;
                                    }
                                    state.resize_panel(1, height, window, cx);
                                    true
                                });
                        root.workbench_bottom_restore_pending = !restored;
                        if restored {
                            cx.notify();
                        }
                    }
                }
                if restore_dock_now {
                    if root.right_dock_collapsed {
                        // Dock 在 deferred frame 前被隐藏时，等它重新挂载后再恢复。
                        root.workbench_dock_restore_pending = dock_width.is_some();
                    } else if restore_sidebar_now {
                        // 侧栏回填会在下一轮布局改变 Dock 边界，故将 Dock 留到紧随其后的一帧。
                        restore_dock_after_sidebar = dock_width.is_some();
                    } else if let Some(width) = dock_width {
                        let restored =
                            root.workbench_dock_resizable_state.update(cx, |state, cx| {
                                if let Some(last_panel_ix) = state.sizes().len().checked_sub(1) {
                                    state.resize_panel(last_panel_ix, width, window, cx);
                                    true
                                } else {
                                    false
                                }
                            });
                        root.workbench_dock_restore_pending = !restored;
                    }
                }
            });

            if restore_dock_after_sidebar && let Some(width) = dock_width {
                let root_entity = root_entity.clone();
                window.on_next_frame(move |window, app| {
                    root_entity.update(app, |root, cx| {
                        if root.workbench_layout_restore_generation != generation {
                            return;
                        }
                        if root.right_dock_collapsed {
                            root.workbench_dock_restore_pending = true;
                            return;
                        }
                        let restored =
                            root.workbench_dock_resizable_state.update(cx, |state, cx| {
                                if let Some(last_panel_ix) = state.sizes().len().checked_sub(1) {
                                    state.resize_panel(last_panel_ix, width, window, cx);
                                    true
                                } else {
                                    false
                                }
                            });
                        root.workbench_dock_restore_pending = !restored;
                    });
                });
            }
        });
    }
    let center_area_min_width = layout_plan.center_min_width
        + if !compact_panels && panels.left.is_some() {
            LEFT_TOOL_PANEL_MIN_WIDTH + RESIZABLE_EDGE_GUTTER * 2.0
        } else {
            0.0
        };
    // 左栏的半槽与中心的半槽合成 8px；右栏收起时由中心独自补齐，隐藏左栏则保持两侧 4px。
    let closed_dock_gutter = if effective_host_sidebar_collapsed {
        RESIZABLE_EDGE_GUTTER
    } else {
        RESIZABLE_EDGE_GUTTER * 2.0
    };
    let main_workspace_min_width = center_area_min_width
        + if dock_is_visible {
            workbench_layout::ATTACHED_DOCK_CONTENT_MIN_WIDTH + RESIZABLE_EDGE_GUTTER * 3.0
        } else {
            RESIZABLE_EDGE_GUTTER + closed_dock_gutter
        };

    let main_workspace = if compact_panels {
        div()
            .size_full()
            .pl(px(RESIZABLE_EDGE_GUTTER))
            .pr(px(closed_dock_gutter))
            .child(center_area)
            .into_any_element()
    } else if dock_is_visible {
        let dock_content = agent_dock_element
            .take()
            .unwrap_or_else(|| repeated_tool_panel_renderer(dock_tool));
        h_resizable("workbench-attached-dock-split")
            .preserve_panel_size_on_container_resize(1)
            .with_state(&root.workbench_dock_resizable_state)
            .with_handle_appearance(resize_handle_appearance.clone())
            .on_resize({
                let root_entity = cx.entity();
                move |state, _, app| {
                    let width = state
                        .read(app)
                        .preferred_panel_size(1)
                        .or_else(|| state.read(app).sizes().last().copied());
                    if let Some(width) = width {
                        root_entity.update(app, |root, cx| {
                            root.record_user_attached_width(dock_tool, width, cx);
                        });
                    }
                }
            })
            .child(
                resizable_panel()
                    .size_range(
                        px(center_area_min_width + RESIZABLE_EDGE_GUTTER * 2.0)..px(4_000.0),
                    )
                    .child(
                        div()
                            .size_full()
                            .px(px(RESIZABLE_EDGE_GUTTER))
                            .child(center_area),
                    ),
            )
            .child(
                resizable_panel()
                    .size(initial_dock_width)
                    .size_range(
                        px(workbench_layout::ATTACHED_DOCK_CONTENT_MIN_WIDTH
                            + RESIZABLE_EDGE_GUTTER)
                            ..px(layout_plan.attached_dock_max_width + RESIZABLE_EDGE_GUTTER),
                    )
                    // 保留用户拖动后的面板基准尺寸，避免中心区受最小宽度约束时 Dock 继续吸收剩余空间。
                    .flex_none()
                    .child(
                        div()
                            .size_full()
                            .pl(px(RESIZABLE_EDGE_GUTTER))
                            .child(dock_content),
                    ),
            )
            .into_any_element()
    } else {
        div()
            .size_full()
            .pl(px(RESIZABLE_EDGE_GUTTER))
            .pr(px(closed_dock_gutter))
            .child(center_area)
            .into_any_element()
    };

    let (main_columns, main_columns_min_width) = if effective_host_sidebar_collapsed {
        let min_width = main_workspace_min_width;
        let columns = div()
            .id("workbench-sidebar-collapsed-columns")
            .size_full()
            .flex()
            .child(
                div()
                    .id("workbench-main-workspace-collapsed")
                    .h_full()
                    .flex_1()
                    .min_w(px(main_workspace_min_width))
                    .child(main_workspace),
            )
            .into_any_element();
        (columns, min_width)
    } else {
        let min_width =
            HOST_NAVIGATION_MIN_WIDTH + RESIZABLE_EDGE_GUTTER + main_workspace_min_width;
        let columns = h_resizable("workbench-sidebar-split")
            .preserve_panel_size_on_container_resize(0)
            .with_state(&root.workbench_sidebar_resizable_state)
            .with_handle_appearance(resize_handle_appearance)
            .on_resize({
                let root_entity = cx.entity();
                move |state, _, app| {
                    let width = state
                        .read(app)
                        .preferred_panel_size(0)
                        .or_else(|| state.read(app).sizes().first().copied());
                    if let Some(width) = width {
                        root_entity
                            .update(app, |root, cx| root.record_user_sidebar_width(width, cx));
                    }
                }
            })
            .child(
                resizable_panel()
                    .size(
                        root.workbench_sidebar_restore_width
                            .unwrap_or(px(HOST_NAVIGATION_WIDTH + RESIZABLE_EDGE_GUTTER)),
                    )
                    .size_range(
                        px(HOST_NAVIGATION_MIN_WIDTH + RESIZABLE_EDGE_GUTTER)
                            ..px(HOST_NAVIGATION_MAX_WIDTH + RESIZABLE_EDGE_GUTTER),
                    )
                    // Kit 面板默认会随兄弟布局伸缩；固定已记录的侧栏基准，避免拖动后在下一帧回缩。
                    .flex_none()
                    .child(
                        div()
                            .size_full()
                            .pr(px(RESIZABLE_EDGE_GUTTER))
                            .child(left_sidebar),
                    ),
            )
            .child(
                resizable_panel()
                    .size_range(px(main_workspace_min_width)..px(4_000.0))
                    .child(main_workspace),
            )
            .into_any_element();
        (columns, min_width)
    };

    let mut workbench_body = div()
        .id("workbench-body")
        .relative()
        .flex_1()
        .min_h(px(WORKBENCH_MIN_HEIGHT))
        .flex()
        .gap_0()
        .px_0()
        .pb_0()
        .overflow_hidden()
        .text_color(theme.foreground)
        .child(
            div()
                .id("workbench-resizable-main")
                .flex_1()
                .min_w(px(main_columns_min_width))
                .min_h(px(WORKBENCH_MIN_HEIGHT))
                .h_full()
                .child(main_columns),
        );

    let mut tool_rail = Some(tool_rail);
    if show_compact_tool_drawer && let Some(tool_rail) = tool_rail.take() {
        workbench_body = workbench_body.child(tool_rail);
    }

    if show_compact_host_drawer {
        let drawer_width =
            (viewport.width.as_f32() - TOOL_RAIL_WIDTH - WORKBENCH_PANEL_GUTTER - 16.0)
                .clamp(0.0, 360.0);
        workbench_body = workbench_body.child(workbench_drawers::render_compact_host_drawer(
            compact_host_navigation,
            drawer_width,
            TOOL_RAIL_WIDTH + WORKBENCH_PANEL_GUTTER,
            theme,
            panel_background,
            &root.compact_drawer_focus_handle,
            cx,
        ));
    }

    if show_compact_tool_drawer && let Some(tool) = compact_active_tool {
        let content = if panels.attached == Some(tool) {
            agent_dock_element
                .take()
                .unwrap_or_else(|| repeated_tool_panel_renderer(tool))
        } else {
            workbench_tool_panel_slot(
                "workbench-compact-tool-drawer-content",
                renderers.take_for(tool),
            )
            .into_any_element()
        };
        workbench_body = workbench_body.child(workbench_drawers::render_compact_tool_drawer(
            content,
            tool,
            workbench_drawers::ToolDrawerLayout {
                left: 0.0,
                right: 0.0,
                width: workbench_drawers::tool_drawer_width(
                    viewport.width.as_f32(),
                    8.0,
                    8.0,
                    440.0,
                ),
            },
            theme,
            theme.tool_panel_floating_surface,
            &root.compact_drawer_focus_handle,
            cx,
        ));
    }

    if let Some(tool) = super::tool_panels::expanded_center_tool_panel(panels, compact_panels) {
        let content = renderers.take_for(tool);
        let close_label = crate::locale::text("关闭工具面板", "Close tool panel");
        let drag_tooltip = crate::locale::text(
            "拖动工具浮窗；方向键可微调位置",
            "Drag the floating panel; use arrow keys to adjust its position",
        );
        let viewport_width = viewport.width.as_f32();
        let body_height = (viewport.height.as_f32() - WORKBENCH_TITLE_BAR_HEIGHT).max(0.0);
        let floating_geometry =
            workbench_layout::floating_panel_geometry(viewport_width, body_height);
        let floating_position = root
            .tool_rail_state
            .floating_position(tool)
            .map(|position| {
                workbench_layout::clamp_floating_panel_position(
                    position,
                    viewport_width,
                    body_height,
                    floating_geometry,
                )
            })
            .unwrap_or_else(|| {
                workbench_layout::initial_floating_panel_position(
                    viewport_width,
                    body_height,
                    floating_geometry,
                )
            });
        root.tool_rail_state
            .set_floating_position(tool, floating_position);
        workbench_body = workbench_body.child(
            div()
                .id("workbench-tool-panel-center-floating")
                .debug_selector(|| "workbench-tool-panel-center-floating".into())
                .absolute()
                .left(px(floating_position.x as f32))
                .top(px(floating_position.y as f32))
                .w(px(floating_geometry.width))
                .h(px(floating_geometry.height))
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded(theme.radius_panel)
                .shadow_lg()
                .border_1()
                .border_color(theme.tool_panel_border)
                .bg(theme.tool_panel_surface)
                .on_key_down(cx.listener(move |root, event, window, cx| {
                    root.handle_center_tool_panel_key_down(tool, event, window, cx);
                }))
                .child(
                    div()
                        .id("workbench-tool-panel-center-header")
                        .w_full()
                        .h(px(40.))
                        .flex_shrink_0()
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(theme.tool_panel_border)
                        .text_color(theme.foreground)
                        .child(
                            div()
                                .id("workbench-tool-panel-center-drag-handle")
                                .debug_selector(|| "workbench-tool-panel-center-drag-handle".into())
                                .flex_1()
                                .min_w(px(0.))
                                .h_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .cursor(if root.tool_rail_state.floating_drag_is_active() {
                                    gpui_kit::CursorStyle::ClosedHand
                                } else {
                                    gpui_kit::CursorStyle::OpenHand
                                })
                                .role(gpui_kit::Role::Button)
                                .tab_index(0)
                                .tab_stop(true)
                                .aria_label(crate::locale::text(
                                    "拖动工具浮窗",
                                    "Drag floating tool panel",
                                ))
                                .aria_keyshortcuts("ArrowLeft ArrowRight ArrowUp ArrowDown")
                                .tooltip(move |window, cx| {
                                    Tooltip::new(drag_tooltip.clone()).build(window, cx)
                                })
                                .focus_visible(|style| style.border_color(theme.accent))
                                .on_key_down(cx.listener(
                                    move |root, event: &KeyDownEvent, window, cx| {
                                        root.handle_floating_tool_panel_key_down(
                                            tool,
                                            FloatingPanelKeyLayout {
                                                rendered_position: floating_position,
                                                viewport_width,
                                                body_height,
                                                geometry: floating_geometry,
                                            },
                                            event,
                                            window,
                                            cx,
                                        );
                                    },
                                ))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |root, event: &MouseDownEvent, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                        root.tool_rail_state.begin_floating_drag(
                                            tool,
                                            event.position.x.as_f32(),
                                            event.position.y.as_f32() - WORKBENCH_TITLE_BAR_HEIGHT,
                                            floating_position,
                                        );
                                        cx.notify();
                                    }),
                                )
                                .child(
                                    Icon::new(IconName::GripVertical)
                                        .rotate(gpui_kit::radians(std::f32::consts::FRAC_PI_2))
                                        .size(px(16.)),
                                )
                                .child(tool_rail_label(tool)),
                        )
                        .child(
                            Button::new("workbench-close-center-tool-panel")
                                .compact()
                                .ghost()
                                .icon(IconName::X)
                                .accessibility_label(close_label.clone())
                                .tooltip(close_label)
                                .tab_stop(true)
                                .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                                    root.dismiss_center_tool_panel(tool, window, cx);
                                })),
                        ),
                )
                .child(workbench_tool_panel_slot(
                    "workbench-tool-panel-center",
                    content,
                )),
        );
    }

    if root.tool_rail_state.floating_drag_is_active() {
        let root_entity = cx.entity();
        let viewport_width = viewport.width.as_f32();
        let body_height = (viewport.height.as_f32() - WORKBENCH_TITLE_BAR_HEIGHT).max(0.0);
        let floating_geometry =
            workbench_layout::floating_panel_geometry(viewport_width, body_height);
        workbench_body = workbench_body.child(
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let root_entity_for_move = root_entity.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, app| {
                        if !phase.bubble() || !event.dragging() {
                            return;
                        }
                        let x = event.position.x.as_f32();
                        let y = event.position.y.as_f32() - WORKBENCH_TITLE_BAR_HEIGHT;
                        root_entity_for_move.update(app, |root, cx| {
                            if root.tool_rail_state.update_floating_drag(
                                x,
                                y,
                                viewport_width,
                                body_height,
                                floating_geometry,
                            ) {
                                cx.notify();
                            }
                        });
                    });
                    let root_entity_for_key = root_entity.clone();
                    window.on_key_event(move |event: &KeyDownEvent, phase, window, app| {
                        if !phase.capture() || !event.keystroke.key.eq_ignore_ascii_case("escape") {
                            return;
                        }
                        root_entity_for_key.update(app, |root, cx| {
                            if root.tool_rail_state.cancel_floating_drag() {
                                window.prevent_default();
                                cx.stop_propagation();
                                cx.notify();
                            }
                        });
                    });
                    let root_entity_for_up = root_entity.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, app| {
                        if phase.bubble() && event.button == MouseButton::Left {
                            root_entity_for_up.update(app, |root, cx| {
                                if root.tool_rail_state.finish_floating_drag() {
                                    cx.notify();
                                }
                            });
                        }
                    });
                },
            )
            .absolute()
            .inset_0(),
        );
    }

    let feedback_max_width = (viewport.width.as_f32() - TOOL_RAIL_WIDTH - 32.0).max(0.0);
    if let Some(feedback) =
        render_workbench_feedback(root, theme, panel_background, feedback_max_width, cx)
    {
        workbench_body = workbench_body.child(feedback);
    }
    if let Some(tool_rail) = tool_rail {
        workbench_body = workbench_body.child(tool_rail);
    }

    let mut workbench = div()
        .id("kerminal-workbench")
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .font_family(ui_font_family)
        .text_size(px(14.))
        .text_color(theme.foreground)
        .child(title_bar)
        .child(workbench_body);
    if root.preferences_modal_open
        && !root.connection_prompt_modal_open
        && root.root_confirmation_owner.is_none()
    {
        let focus_handle = root.preferences_modal_focus_handle.clone();
        let preferences_panel = root.preferences_panel.clone();
        workbench = workbench.child(crate::ui_backdrop::modal_backdrop(
            div()
                .id("workbench-preferences-modal-overlay")
                .absolute()
                .inset_0()
                .occlude()
                .pl(px(16.))
                .pr(px(16.))
                .pt(px(24.))
                .pb(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.dialog_scrim)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|root, _: &MouseDownEvent, _, cx| {
                        root.request_close_preferences_modal(cx);
                    }),
                )
                .child(crate::ui_backdrop::floating_surface(
                    div()
                        .id("workbench-preferences-modal-surface")
                        .w_full()
                        .h_full()
                        .max_w(px(1152.))
                        .max_h(px(780.))
                        .min_w(px(0.))
                        .min_h(px(0.))
                        .occlude()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .rounded(px(20.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.floating_surface)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(preferences_panel)
                        .focus_trap("workbench-preferences-focus-trap", &focus_handle),
                    theme,
                    theme.radius_dialog,
                )),
            theme,
        ));
    } else if root.connection_prompt_modal_open
        || root.connections_modal_open && root.root_confirmation_owner.is_none()
    {
        if let Some(connections_panel) = root.connections_panel.clone() {
            let focus_handle = root.connections_modal_focus_handle.clone();
            let editor_size = crate::features::connections::host_editor_size(window);
            let nested_editor_open = connections_panel.read(cx).nested_editor_open();
            let surface = div()
                .id("workbench-connections-modal-surface")
                .w(editor_size.width)
                .h(editor_size.height)
                .min_w(px(0.))
                .min_h(px(0.))
                .occlude()
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(connections_panel);
            // 嵌套任务独占焦点陷阱；同时注册父子容器会让 Kit 的活跃陷阱选择产生歧义。
            let surface = if nested_editor_open {
                surface.into_any_element()
            } else {
                surface
                    .focus_trap("workbench-connections-focus-trap", &focus_handle)
                    .into_any_element()
            };
            workbench = workbench.child(
                div()
                    .id("workbench-connections-modal-overlay")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .p_4()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.38))
                    .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
                        root.request_close_connection_editor(window, cx);
                    }))
                    .child(surface),
            );
        }
    } else if root.root_confirmation_owner.is_none()
        && snippet_editor_open
        && let Some(snippet_editor) = super::snippet_editor::render_overlay(root, theme, window, cx)
    {
        workbench = workbench.child(snippet_editor);
    }
    if let Some(manager) = super::custom_agent_manager::render_overlay(root, theme, window, cx) {
        workbench = workbench.child(manager);
    }
    if root.root_confirmation_owner.is_some() && !root.connection_prompt_modal_open {
        // 运维与文件弹框复刻 main ModalShell；编辑器退出确认继续使用其独立宿主外观。
        let rail_dialog = matches!(
            root.root_confirmation_owner,
            Some(RootConfirmationOwner::Operations | RootConfirmationOwner::FileDelete)
        );
        let confirmation_width = match root.root_confirmation_owner {
            Some(RootConfirmationOwner::Operations) => root
                .root_confirmation_operations_view
                .as_ref()
                .map_or(680., |view| view.read(cx).root_confirmation_max_width()),
            Some(RootConfirmationOwner::FileDelete) => root
                .root_confirmation_file_pane
                .as_ref()
                .map_or(576., |pane| pane.read(cx).global_dialog_max_width()),
            _ => 680.,
        };
        // main 的普通 ModalShell 使用 16px 外围留白与 48px 高度安全区；表单按自然内容收缩。
        let confirmation_padding = if rail_dialog { 16. } else { 20. };
        let confirmation_height = if rail_dialog {
            (viewport.height.as_f32() - 48.0).clamp(0.0, 704.0)
        } else {
            (viewport.height.as_f32() - 40.0).clamp(0.0, 720.0)
        };
        let confirmation = match root.root_confirmation_owner {
            Some(RootConfirmationOwner::Operations) => root
                .root_confirmation_operations_view
                .clone()
                .and_then(|operations| {
                    operations.update(cx, |view, cx| {
                        view.render_root_confirmation(
                            theme,
                            (viewport.width.as_f32() - confirmation_padding * 2.)
                                .min(confirmation_width),
                            (confirmation_height - 2.0).max(0.0),
                            cx,
                        )
                    })
                }),
            Some(RootConfirmationOwner::FileDelete) => root
                .root_confirmation_file_pane
                .clone()
                .and_then(|pane| pane.update(cx, |pane, cx| pane.render_global_dialog(window, cx))),
            Some(RootConfirmationOwner::Editor) => {
                root.root_confirmation_editor.clone().and_then(|editor| {
                    editor.update(cx, |editor, cx| editor.render_root_confirmation(theme, cx))
                })
            }
            None => None,
        };
        if let Some(confirmation) = confirmation {
            let focus_handle = root.connections_modal_focus_handle.clone();
            workbench = workbench.child(
                div()
                    .id("workbench-root-confirmation-overlay")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .p(px(confirmation_padding))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if rail_dialog {
                        theme.dialog_scrim
                    } else {
                        gpui_kit::hsla(0.0, 0.0, 0.0, 0.38)
                    })
                    .on_click(cx.listener(|root, _: &ClickEvent, _, cx| {
                        root.cancel_root_confirmation(cx);
                    }))
                    .child(
                        div()
                            .id("workbench-root-confirmation-surface")
                            .w_full()
                            .max_w(px(confirmation_width))
                            .max_h(px(confirmation_height))
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .occlude()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .rounded(if rail_dialog {
                                theme.radius_dialog
                            } else {
                                px(8.)
                            })
                            .border_1()
                            .border_color(theme.border)
                            .bg(if rail_dialog {
                                theme.floating_surface
                            } else {
                                panel_background
                            })
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(
                                div()
                                    .id("workbench-root-confirmation-content")
                                    .w_full()
                                    .flex_1()
                                    .min_h(px(0.))
                                    .overflow_y_scroll()
                                    .child(confirmation),
                            )
                            .focus_trap("workbench-root-confirmation-focus-trap", &focus_handle)
                            .map(|surface| {
                                if rail_dialog {
                                    crate::ui_backdrop::floating_surface(
                                        surface,
                                        theme,
                                        theme.radius_dialog,
                                    )
                                } else {
                                    surface.into_any_element()
                                }
                            }),
                    )
                    .map(|overlay| {
                        if rail_dialog {
                            crate::ui_backdrop::modal_backdrop(overlay, theme)
                        } else {
                            overlay.into_any_element()
                        }
                    }),
            );
        }
    }
    workbench
}

/// 仅本次计算的只读投影；不增加 Root 缓存、延迟重试或第二份响应式状态。
struct WorkbenchViewportProjection {
    plan: workbench_layout::WorkbenchLayoutPlan,
    failed_file_tab: Option<(TabId, gpui_kit::SharedString)>,
    workspace_notice: Option<gpui_kit::SharedString>,
    recovery_max_height: f32,
}

/// 每次只读构造当前视口事实，不缓存布局意图；首次打开与绘制必须共用同一预算，避免首击只收左栏。
/// 浮动状态提示不占终端高度；恢复诊断、拖动首选尺寸与 Agent 真实绑定仍沿用原规则。
///
/// @author kongweiguang
fn project_workbench_viewport(
    root: &RootView,
    panels: ToolRailPanelSlots,
    window: &Window,
    cx: &Context<RootView>,
) -> WorkbenchViewportProjection {
    // 先查询当前文件标签的恢复失败状态，使布局预算与稍后显示的重试行一致。
    let failed_file_tab = root.workspace_controller.as_ref().and_then(|controller| {
        let snapshot = controller.snapshot();
        let tab_id = snapshot.active_tab_id.as_ref()?;
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let expected_panel = match tab {
            WorkspaceSessionTab::WorkspaceFile { .. } => CenterPanel::Editor,
            WorkspaceSessionTab::SftpTransfer { .. } => {
                if root.files_workspace_selection
                    != super::workspace_files::FilesWorkspaceSelection::TransferTab(tab_id.clone())
                {
                    return None;
                }
                CenterPanel::Files
            }
            WorkspaceSessionTab::Terminal { .. } | WorkspaceSessionTab::Unsupported { .. } => {
                return None;
            }
        };
        if root.center_panel != expected_panel {
            return None;
        }
        match root.workspace_files.restore_state(tab_id)? {
            WorkspaceFileRestoreState::Failed { message, .. } => {
                Some((tab_id.clone(), message.clone()))
            }
            WorkspaceFileRestoreState::Pending | WorkspaceFileRestoreState::Ready => None,
        }
    });

    // 激活与绘制均按实际视口和用户首选尺寸计算；底部记录栏已移除，不再从垂直预算扣高。
    // 从不把窗口夹紧值写回 ResizableState。
    let viewport = window.viewport_size();
    let host_sidebar_preferred_width = {
        let state = root.workbench_sidebar_resizable_state.read(cx);
        root.workbench_sidebar_restore_width
            .filter(|_| root.workbench_sidebar_restore_pending)
            .or_else(|| state.preferred_panel_size(0))
            .or_else(|| state.sizes().first().copied())
            .unwrap_or(px(HOST_NAVIGATION_WIDTH + RESIZABLE_EDGE_GUTTER))
            .as_f32()
    };
    let attached_dock_preferred_width = panels
        .attached
        .map(|tool| {
            root.tool_rail_state
                .attached_width(tool)
                .unwrap_or_else(|| root.current_attached_default_width(tool))
                .as_f32()
        })
        .unwrap_or(workbench_layout::ATTACHED_DOCK_CONTENT_MIN_WIDTH + RESIZABLE_EDGE_GUTTER);
    let left_tool_panel_preferred_width = root
        .workbench_left_tool_resizable_state
        .read(cx)
        .preferred_panel_size(0)
        .or_else(|| {
            root.workbench_left_tool_resizable_state
                .read(cx)
                .sizes()
                .first()
                .copied()
        })
        .unwrap_or_else(|| {
            px(
                workbench_layout::initial_tool_panel_width(viewport.width.as_f32())
                    + RESIZABLE_EDGE_GUTTER,
            )
        })
        .as_f32();
    let workspace_notice = super::workspace_session_notice(
        root.workspace_session_state.as_ref(),
        root.workspace_session_error.as_deref(),
    );
    let workspace_notice_height = if workspace_notice.is_some() {
        40.0
    } else {
        0.0
    };
    let file_restore_notice_height = if failed_file_tab.is_some() { 40.0 } else { 0.0 };
    // 恢复诊断保留滚动入口；浮层不占高，预算仅为正文恢复诊断预留空间。
    let recovery_max_height = (viewport.height.as_f32()
        - 36.0
        - 8.0
        - workbench_layout::CENTER_MIN_HEIGHT
        - workspace_notice_height
        - file_restore_notice_height)
        .max(0.0)
        .min(viewport.height.as_f32() * 0.25)
        .max(super::RECOVERY_BANNER_HEADER_HEIGHT);
    let recovery_height =
        super::recovery_banner_height(&root.configuration_recovery).min(recovery_max_height);
    let reserved_vertical_height =
        recovery_height + workspace_notice_height + file_restore_notice_height;
    let layout_plan =
        workbench_layout::resolve_workbench_layout(&workbench_layout::WorkbenchLayoutInput {
            viewport_width: viewport.width.as_f32(),
            viewport_height: viewport.height.as_f32(),
            host_sidebar_collapsed: root.host_sidebar_collapsed,
            right_dock_collapsed: root.right_dock_collapsed,
            host_sidebar_preferred_width,
            attached_dock_preferred_width,
            left_tool_panel_preferred_width,
            left_tool_open: panels.left.is_some(),
            attached_tool_open: panels.attached.is_some(),
            bottom_tool_open: panels.bottom.is_some(),
            focused_tools_height: 0.0,
            focused_tools_expanded_height: 0.0,
            status_bar_height: 0.0,
            reserved_vertical_height,
            open_tool_order: &root.tool_rail_state.open_order,
        });
    WorkbenchViewportProjection {
        plan: layout_plan,
        failed_file_tab,
        workspace_notice,
        recovery_max_height,
    }
}

/// 将 Root 的状态变更投影成可见时长为 4 秒的提示，迟到计时器不能清掉后续新消息。
/// 模态层遮住工作区时暂停计时；显式观察到 None 也推进代次，因此相同文本重新出现会重计。
///
/// @author kongweiguang
fn track_workbench_status_toast(root: &mut RootView, obscured: bool, cx: &mut Context<RootView>) {
    if obscured {
        if root.status_toast_observed.is_some() {
            root.status_toast_revision = root.status_toast_revision.wrapping_add(1).max(1);
            root.status_toast_observed = None;
        }
        return;
    }

    let observed = root.status.as_ref().map(ToString::to_string);
    if root.status_toast_observed == observed {
        return;
    }

    root.status_toast_revision = root.status_toast_revision.wrapping_add(1).max(1);
    root.status_toast_observed = observed.clone();
    let Some(message) = observed else {
        return;
    };
    let revision = root.status_toast_revision;
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(Duration::from_secs(4)).await;
        let _ = this.update(cx, |root, cx| {
            let same_message = root
                .status
                .as_ref()
                .is_some_and(|status| *status == message);
            if root.status_toast_revision != revision || !same_message {
                return;
            }
            root.status = None;
            root.status_toast_observed = None;
            root.status_toast_revision = root.status_toast_revision.wrapping_add(1).max(1);
            cx.notify();
        });
    })
    .detach();
}

/// 用短暂浮层替代常驻状态栏；普通状态可关闭并自动消失，布局保存异常仍带恢复入口。
/// 宽度按真实视口扣除 ToolRail 与两侧留白，恢复操作允许换行以免窄窗溢出。
/// 同时显示一般状态与布局错误，保留设置的反馈间距密度，避免持久的布局问题遮住其他操作反馈。
///
/// @author kongweiguang
fn render_workbench_feedback(
    root: &RootView,
    theme: ThemeTokens,
    panel_background: gpui_kit::Hsla,
    max_width: f32,
    cx: &mut Context<RootView>,
) -> Option<AnyElement> {
    let density = super::preferences::interface_density_metrics(root);
    let status = root.status.clone();
    let layout_error = root.workbench_layout_save_error.clone();
    let has_pending_layout = root.workbench_layout_intent.is_some();
    let has_layout_feedback = has_pending_layout || layout_error.is_some();
    if status.is_none() && !has_layout_feedback {
        return None;
    }

    let layout_in_flight = has_pending_layout
        && root
            .preferences_panel
            .read(cx)
            .is_workbench_layout_save_in_flight();
    let mut feedback = div()
        .id("workbench-feedback-toast")
        .absolute()
        .bottom(px(12.))
        .right(px(TOOL_RAIL_WIDTH + 16.0))
        .max_w(px(max_width))
        .min_w(px(max_width.min(220.)))
        .flex()
        .flex_col()
        .gap(px(density.item_gap / 2.0))
        .px(px(density.item_padding_x))
        .py(px(density.item_padding_y))
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(panel_background);

    if let Some(error) = layout_error {
        feedback = feedback.child(
            div()
                .id("workbench-layout-error-message")
                .text_size(theme.text_xs)
                .text_color(theme.error)
                .child(error),
        );
    }

    if has_layout_feedback {
        let saving_label = if layout_in_flight {
            crate::locale::text("正在保存布局", "Saving layout")
        } else {
            crate::locale::text("布局未保存", "Layout unsaved")
        };
        feedback = feedback.child(
            div()
                .id("workbench-layout-pending-controls")
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(saving_label)
                .child(
                    Button::new("workbench-layout-open-settings")
                        .ghost()
                        .compact()
                        .with_size(px(28.))
                        .label(crate::locale::text("设置读回", "Settings"))
                        .disabled(layout_in_flight)
                        .on_click(cx.listener(RootView::open_preferences)),
                )
                .child(
                    Button::new("workbench-layout-confirm-readback")
                        .ghost()
                        .compact()
                        .with_size(px(28.))
                        .label(crate::locale::text("核对", "Verify"))
                        .disabled(layout_in_flight)
                        .on_click(cx.listener(|root, _, _, cx| {
                            root.confirm_workbench_layout_readback(cx);
                        })),
                )
                .child(
                    Button::new("workbench-layout-retry")
                        .ghost()
                        .compact()
                        .with_size(px(28.))
                        .label(crate::locale::text("重试", "Retry"))
                        .disabled(layout_in_flight)
                        .on_click(cx.listener(|root, _, _, cx| {
                            let layout = root.current_workbench_layout_intent();
                            root.request_workbench_layout_intent(layout, cx);
                        })),
                ),
        );
    }

    if let Some(status) = status {
        let dismiss_label = crate::locale::text("关闭提示", "Dismiss notification");
        feedback = feedback.child(
            div()
                .id("workbench-status-toast-message-row")
                .w_full()
                .flex()
                .items_start()
                .gap_2()
                .text_size(theme.text_xs)
                .text_color(theme.foreground)
                .child(div().flex_1().min_w(px(0.)).child(status))
                .child(
                    Button::new("workbench-dismiss-status-toast")
                        .compact()
                        .ghost()
                        .with_size(px(24.))
                        .icon(IconName::X)
                        .accessibility_label(dismiss_label.clone())
                        .tooltip(dismiss_label)
                        .on_click(cx.listener(|root, _, _, cx| {
                            root.status = None;
                            root.status_toast_observed = None;
                            root.status_toast_revision =
                                root.status_toast_revision.wrapping_add(1).max(1);
                            cx.notify();
                        })),
                ),
        );
    }

    Some(feedback.into_any_element())
}

/// 让面板直接占满宿主；工具自身标题和被选中的 Rail 入口已提供身份与关闭路径。
///
/// @author kongweiguang
fn workbench_tool_panel_slot(id: &'static str, content: AnyElement) -> impl IntoElement {
    div()
        .id(id)
        .flex_1()
        .min_h(px(0.))
        .w_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(content)
}

/// 将真实 ToolRail 身份映射成稳定控件 ID，保证重排不会重建面板实体。
///
/// @author kongweiguang
fn tool_rail_button_id(tool: ToolRailToolId) -> &'static str {
    match tool {
        ToolRailToolId::Context => "workbench-tool-context",
        ToolRailToolId::AgentLauncher => "workbench-tool-agent-launcher",
        ToolRailToolId::Sftp => "workbench-tool-sftp",
        ToolRailToolId::Containers => "workbench-tool-containers",
        ToolRailToolId::Snippets => "workbench-tool-snippets",
        ToolRailToolId::Tmux => "workbench-tool-tmux",
        ToolRailToolId::Ports => "workbench-tool-ports",
        ToolRailToolId::System => "workbench-tool-system",
        ToolRailToolId::Logs => "workbench-tool-logs",
    }
}

/// 设置沿用 main 的齿轮图标；独立提示触发器不改变按钮命中区或终端焦点生命周期。
///
/// @author kongweiguang
fn render_settings_button(
    button_id: &'static str,
    tooltip_id: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = crate::locale::text("设置", "Settings");
    let tooltip_label = label.clone();
    div()
        .id(tooltip_id)
        .w(px(32.))
        .h(px(32.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
        .child(
            Button::new(button_id)
                .compact()
                .ghost()
                .icon(IconName::Settings)
                .accessibility_label(label)
                .tab_stop(true)
                .on_click(on_click),
        )
}

/// Rail 与临时抽屉共用名称投影；传稳定 ToolId 同时约束抽屉 Escape owner，不复制标题或增加参数。
///
/// @author kongweiguang
pub(super) fn tool_rail_label(tool: ToolRailToolId) -> gpui_kit::SharedString {
    match tool {
        ToolRailToolId::Context => {
            crate::locale::text("已移除的上下文工具", "Removed context tool")
        }
        ToolRailToolId::AgentLauncher => crate::locale::text("Agent 启动器", "Agent launcher"),
        ToolRailToolId::Sftp => crate::locale::text("文件", "Files"),
        ToolRailToolId::Containers => crate::locale::text("容器", "Containers"),
        ToolRailToolId::Snippets => crate::locale::text("命令片段", "Command snippets"),
        ToolRailToolId::Tmux => crate::locale::text("tmux 会话", "tmux sessions"),
        ToolRailToolId::Ports => crate::locale::text("端口转发", "Port forwards"),
        ToolRailToolId::System => crate::locale::text("服务器信息", "Server information"),
        ToolRailToolId::Logs => crate::locale::text("命令历史", "Command history"),
    }
}

/// 如果状态错误导致同一工具被两个宿主重复取用，显示可见诊断而不重挂 Entity。
///
/// @author kongweiguang
fn repeated_tool_panel_renderer(tool: ToolRailToolId) -> AnyElement {
    div()
        .id("workbench-tool-panel-duplicate-renderer")
        .w_full()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(12.))
        .child(crate::locale::text(
            format!("{} 面板无法在两个区域同时挂载。", tool_rail_label(tool)),
            format!(
                "The {} panel cannot be mounted in two slots at once.",
                tool_rail_label(tool)
            ),
        ))
        .into_any_element()
}

/// 沿用安装版工具的图形语义，避免 tmux 和命令历史与终端、片段入口混淆。
/// 图标继续复用同版本 Kit 资源，稳定工具身份仍由屏幕阅读器单独识别。
///
/// @author kongweiguang
pub(super) fn tool_rail_icon(tool: ToolRailToolId) -> IconName {
    match tool {
        ToolRailToolId::Context => IconName::Link2,
        ToolRailToolId::AgentLauncher => IconName::Bot,
        ToolRailToolId::Sftp => IconName::FolderOpen,
        ToolRailToolId::Containers => IconName::Container,
        ToolRailToolId::Snippets => IconName::FileText,
        ToolRailToolId::Tmux => IconName::PanelsTopLeft,
        ToolRailToolId::Ports => IconName::Network,
        ToolRailToolId::System => IconName::Cpu,
        ToolRailToolId::Logs => IconName::RotateCcwClock,
    }
}

/// 活动状态只由唯一挂载槽推导，避免同一工具因 settings 显示在多个方向。
///
/// @author kongweiguang
fn tool_is_in_panel_slots(tool: ToolRailToolId, slots: ToolRailPanelSlots) -> bool {
    slots.attached == Some(tool)
        || slots.left == Some(tool)
        || slots.bottom == Some(tool)
        || slots.center == Some(tool)
}

/// 右栏按钮尺寸沿用 main 的 density-control-height，同时保留 Kit 的焦点和可访问名称。
///
/// @author kongweiguang
fn workbench_rail_button(
    id: &'static str,
    icon: IconName,
    label: gpui_kit::SharedString,
    button_height: f32,
    selected: bool,
    toggled: Option<bool>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let mut button = Button::new(id)
        .compact()
        .ghost()
        .icon(icon)
        .w(px(button_height))
        .h(px(button_height))
        .selected(selected)
        .accessibility_label(label.clone())
        .tooltip(label)
        .tab_stop(true)
        .on_click(on_click);
    if let Some(toggled) = toggled {
        button = button.toggled(toggled);
    }
    button
}

/// main 的密度令牌同时控制图标按钮宽高，确保各档工具栏节奏一致。
///
/// @author kongweiguang
fn tool_rail_button_height(density: InterfaceDensity) -> f32 {
    match density {
        InterfaceDensity::Compact => 28.0,
        InterfaceDensity::Comfortable => 32.0,
        InterfaceDensity::Spacious => 36.0,
    }
}

/// 非空 Rail 分区至少保留一个完整图标的命中高度；空分区不占用底部空间。
///
/// @author kongweiguang
fn tool_rail_partition_min_height(
    tools: &[ToolRailToolId],
    button_height: f32,
) -> gpui_kit::Pixels {
    px(if tools.is_empty() { 0.0 } else { button_height })
}

/// 判断是否已有活动标签；空终端页只在没有活动标签时提供创建 CTA。
///
/// @author kongweiguang
fn workspace_has_active_tab(root: &RootView) -> bool {
    root.workspace_controller
        .as_ref()
        .is_some_and(|controller| controller.snapshot().active_tab_id.is_some())
}

/// 空工作区只保留终端图标与两个直接入口，避免空白页变成说明卡片。
/// 两个入口都复用 Root 的模态焦点门禁，避免绕过焦点恢复与草稿冲突保护。
///
/// @author kongweiguang
fn empty_terminal_workspace_slot(
    theme: ThemeTokens,
    is_creating: bool,
    can_create_host: bool,
    on_create: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_create_host: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let create_label = crate::locale::text("新建本地终端", "Create local terminal");
    let host_label = crate::locale::text("新建连接", "Create connection");
    let unavailable_label = crate::locale::text(
        "主机连接服务尚未配置，暂时无法新建连接。",
        "Host connection services are unavailable, so a connection cannot be created yet.",
    );
    div()
        .id("workbench-empty-terminal")
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .p_4()
        .text_color(theme.foreground)
        .child(
            Icon::new(IconName::SquareTerminal)
                .large()
                .text_color(theme.muted),
        )
        .child(
            div()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(crate::locale::text(
                    "打开终端开始工作",
                    "Open a terminal to get started",
                )),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Button::new("workbench-empty-terminal-create")
                        .primary()
                        .icon(IconName::Plus)
                        .label(create_label.clone())
                        .accessibility_label(create_label.clone())
                        .tooltip(create_label)
                        .loading(is_creating)
                        .disabled(is_creating)
                        .tab_stop(true)
                        .on_click(on_create),
                )
                .child(
                    Button::new("workbench-empty-terminal-create-host")
                        .secondary()
                        .icon(IconName::Network)
                        .label(host_label.clone())
                        .accessibility_label(host_label.clone())
                        .tooltip(if can_create_host {
                            host_label
                        } else {
                            unavailable_label.clone()
                        })
                        .disabled(!can_create_host)
                        .tab_stop(true)
                        .on_click(on_create_host),
                ),
        )
        .when(!can_create_host, |empty| {
            empty.child(
                div()
                    .max_w(px(420.))
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .text_center()
                    .child(unavailable_label),
            )
        })
        .into_any_element()
}

/// 只把活动终端标签当前明确聚焦且属于该 layout 的 pane 交给底部工具栏。
/// 隐藏工具后的焦点恢复复用同一选择，不能退回分屏中的第一个 pane。
///
/// @author kongweiguang
pub(super) fn focused_workspace_pane(snapshot: &WorkspaceSessionSnapshot) -> Option<PaneId> {
    let active_tab_id = snapshot.active_tab_id.as_ref()?;
    let active_tab = snapshot
        .terminal_tabs
        .iter()
        .find(|tab| tab.id() == active_tab_id)?;
    let layout = active_tab.terminal_layout()?;
    let focused_pane = snapshot.focused_pane_id.as_ref()?;
    layout
        .contains_pane(focused_pane)
        .then(|| focused_pane.clone())
}

#[cfg(test)]
#[path = "../../tests/unit/workbench.rs"]
mod tests;
