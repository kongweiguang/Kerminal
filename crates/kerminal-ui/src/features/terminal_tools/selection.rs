//! 终端选择坐标映射和复制操作控件，不自行解码字符宽度或读取会话资源。
//!
//! @author kongweiguang

use gpui_kit::component::Sizable as _;
use gpui_kit::{
    ClickEvent, Context, EventEmitter, IntoElement, Render, Window, component::button::Button, div,
    prelude::*, px,
};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::identity::SessionId;
use kerminal_terminal::snapshot::{
    RenderSnapshot, SelectionViewport, TerminalPoint, TerminalRow, TerminalSelection,
    TerminalSelectionKind,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::theme::ThemeTokens;
use crate::ui_button::icon_button;

static NEXT_TERMINAL_SELECTION_GESTURE_ID: AtomicU64 = AtomicU64::new(1);

/// 从已换算成终端网格单元的指针坐标开始一次拖动选择。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewportCell {
    pub row: u16,
    pub column: u16,
}

/// 跨 GPUI 重绘保存绝对网格起点，避免 scrollback 移动后将起点解释成另一行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewportSelectionDrag {
    session_id: SessionId,
    binding: Option<UiSessionBinding>,
    generation: u64,
    gesture_id: u64,
    next_scroll_request_id: u64,
    start: TerminalPoint,
    last_end: TerminalPoint,
    last_viewport: Option<SelectionViewport>,
    selection_kind: TerminalSelectionKind,
    last_cell: Option<ViewportCell>,
    edge_direction: i8,
    pending_scroll: Option<PendingSelectionScroll>,
    pending_release: Option<PendingSelectionRelease>,
    open_link_on_click: bool,
    clear_selection_on_click: bool,
}

/// 把拖动回调绑定到完整 session owner 与代次，迟到的 next-frame 不能滚动新目标。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSelectionGestureToken {
    binding: UiSessionBinding,
    generation: u64,
    gesture_id: u64,
}

impl TerminalSelectionGestureToken {
    /// 返回手势开始时冻结的 owner，Root 每帧用它拒绝切 pane 或重连后的迟到 callback。
    ///
    /// @author kongweiguang
    pub(crate) fn binding(&self) -> &UiSessionBinding {
        &self.binding
    }

    /// 代次只对当前未取消手势有效，不能被下一次拖选复用。
    ///
    /// @author kongweiguang
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// 全局 id 隔离销毁重建的同 session ToolsView，engine 只用它固定一次拖选 anchor。
    ///
    /// @author kongweiguang
    pub(crate) fn for_canvas_gesture(binding: UiSessionBinding, generation: u64) -> Self {
        let gesture_id = NEXT_TERMINAL_SELECTION_GESTURE_ID
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        Self {
            binding,
            generation,
            gesture_id,
        }
    }

    /// 把全局唯一的手势 id 传给 owner-bound selection API，不以 session 内局部代次代替。
    ///
    /// @author kongweiguang
    pub(crate) fn gesture_id(&self) -> u64 {
        self.gesture_id
    }
}

/// 一帧自动滚动只暴露本地选择所需的数据，不把 Canvas 或 GPUI 对象存进工具状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalSelectionGestureFrame {
    pub start: TerminalPoint,
    pub cell: ViewportCell,
    pub selection_kind: TerminalSelectionKind,
    pub edge_direction: i8,
    pub gesture_id: u64,
    pub pending_scroll: Option<PendingSelectionScroll>,
    pub pending_release: Option<PendingSelectionRelease>,
}

/// 自动滚动请求收到 ACK 且最小快照 revision 到达前，禁止用旧 viewport 行解释新的选择端点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingSelectionScroll {
    pub offset: usize,
    pub requested_at: Instant,
    pub request_id: u64,
    pub min_snapshot_revision: Option<u64>,
}

impl PendingSelectionScroll {
    /// 仅在该请求收到真实 ACK 且 Canvas 已持有对应或更新的快照后放行端点映射。
    ///
    /// @author kongweiguang
    pub(crate) fn is_acknowledged_by(&self, snapshot_revision: u64) -> bool {
        self.min_snapshot_revision
            .is_some_and(|minimum| snapshot_revision >= minimum)
    }
}

/// 释放发生在视口滚动 ACK 前时保存终点意图，避免把旧 viewport cell 固化成错误绝对行。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingSelectionRelease {
    pub cell: ViewportCell,
    pub plain_unmodified_release: bool,
    pub modifier_is_held: bool,
    pub click_count: usize,
}

impl ViewportSelectionDrag {
    /// 保留原有拖动 API；未显式请求时绝不产生点击清理意图。
    ///
    /// @author kongweiguang
    pub fn new(session_id: SessionId, start: TerminalPoint, open_link_on_click: bool) -> Self {
        Self::new_with_clear_selection_on_click(session_id, start, open_link_on_click, false)
    }

    /// 保存绝对起点、链接手势和已核验的清理意图；实际清除延至 MouseUp，避免与拖选回执竞态。
    ///
    /// @author kongweiguang
    pub fn new_with_clear_selection_on_click(
        session_id: SessionId,
        start: TerminalPoint,
        open_link_on_click: bool,
        clear_selection_on_click: bool,
    ) -> Self {
        Self {
            session_id,
            binding: None,
            generation: 0,
            gesture_id: 0,
            next_scroll_request_id: 1,
            start,
            last_end: start,
            last_viewport: None,
            selection_kind: TerminalSelectionKind::Character,
            last_cell: None,
            edge_direction: 0,
            pending_scroll: None,
            pending_release: None,
            open_link_on_click,
            clear_selection_on_click,
        }
    }

    /// Canvas 手势冻结完整 owner、起点、几何类型和点击策略；generation 在取消后拒绝迟到帧。
    /// 局部保留这组按下事件参数，避免扩散单一构造点的包装类型，也防止坐标与语义错配。
    ///
    /// @author kongweiguang
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_canvas_gesture(
        binding: UiSessionBinding,
        generation: u64,
        gesture_id: u64,
        start: TerminalPoint,
        cell: ViewportCell,
        selection_kind: TerminalSelectionKind,
        open_link_on_click: bool,
        clear_selection_on_click: bool,
    ) -> Self {
        Self {
            session_id: binding.session_id.clone(),
            binding: Some(binding),
            generation,
            gesture_id,
            next_scroll_request_id: 1,
            start,
            last_end: start,
            last_viewport: None,
            selection_kind,
            last_cell: Some(cell),
            edge_direction: 0,
            pending_scroll: None,
            pending_release: None,
            open_link_on_click,
            clear_selection_on_click,
        }
    }

    /// 端点或其投影视口变化时才提交；保留单行 Arc 证据可识别同 cell 的持续输出而不复制历史。
    ///
    /// @author kongweiguang
    pub(crate) fn update(
        &mut self,
        end: TerminalPoint,
        cell: ViewportCell,
        edge_direction: i8,
        viewport: &SelectionViewport,
    ) -> Option<TerminalSelection> {
        self.last_cell = Some(cell);
        self.edge_direction = edge_direction.signum();
        // Arc clone 只增加当前端点行证据的引用计数；PartialEq 比较证据内容，避免每帧重发，
        // 同时覆盖 scroll origin、grid epoch、offset 或同一坐标下行内容改变的投影失效。
        let viewport_changed = self.last_viewport.as_ref() != Some(viewport);
        self.last_viewport = Some(viewport.clone());
        let changed = self.last_end != end || viewport_changed;
        self.last_end = end;
        if !changed {
            return None;
        }
        self.selection_for(end).or_else(|| {
            // 空字符范围仍是有效 Canvas anchor 更新：输出可使同一屏幕 cell 投影到新绝对行，
            // 必须让 engine 按最新 viewport 重定位端点；MouseUp 仍按零长度点击规则处理。
            (self.start == end && self.selection_kind == TerminalSelectionKind::Character)
                .then_some(TerminalSelection {
                    start: self.start,
                    end,
                    kind: self.selection_kind,
                })
        })
    }

    /// 自动滚动 callback 仅在同一完整绑定与代次仍活跃时读取当前锚点和边缘方向。
    ///
    /// @author kongweiguang
    pub(crate) fn frame_state(
        &self,
        binding: &UiSessionBinding,
        generation: u64,
    ) -> Option<TerminalSelectionGestureFrame> {
        if self.binding.as_ref() != Some(binding) || self.generation != generation {
            return None;
        }
        Some(TerminalSelectionGestureFrame {
            start: self.start,
            cell: self.last_cell?,
            selection_kind: self.selection_kind,
            edge_direction: self.edge_direction,
            gesture_id: self.gesture_id,
            pending_scroll: self.pending_scroll,
            pending_release: self.pending_release,
        })
    }

    /// 只更新指针和边缘位置，不在滚动 ACK 前把旧 viewport 的端点投影到引擎选择。
    ///
    /// @author kongweiguang
    pub(crate) fn update_pointer(&mut self, cell: ViewportCell, edge_direction: i8) {
        self.last_cell = Some(cell);
        self.edge_direction = edge_direction.signum();
    }

    /// MouseUp 在途滚动 ACK 前只记录 release 意图；新 offset 到达后再换算绝对 endpoint。
    ///
    /// @author kongweiguang
    pub(crate) fn defer_release_until_scroll_ack(
        &mut self,
        cell: ViewportCell,
        plain_unmodified_release: bool,
        modifier_is_held: bool,
        click_count: usize,
    ) {
        self.update_pointer(cell, 0);
        self.pending_release = Some(PendingSelectionRelease {
            cell,
            plain_unmodified_release,
            modifier_is_held,
            click_count,
        });
    }

    /// 为同一 owner 的当前手势分配单调 request id 并保存唯一在途滚动；ACK 由 Root 回填。
    ///
    /// @author kongweiguang
    pub(crate) fn register_pending_scroll(
        &mut self,
        binding: &UiSessionBinding,
        gesture_id: u64,
        offset: usize,
        requested_at: Instant,
    ) -> Option<u64> {
        if self.binding.as_ref() != Some(binding)
            || self.gesture_id != gesture_id
            || self.pending_scroll.is_some()
        {
            return None;
        }
        let request_id = self.next_scroll_request_id;
        self.next_scroll_request_id = request_id.checked_add(1)?;
        self.pending_scroll = Some(PendingSelectionScroll {
            offset,
            requested_at,
            request_id,
            min_snapshot_revision: None,
        });
        Some(request_id)
    }

    /// 只接受当前在途滚动的真实 Root ACK；快照 offset 不参与确认，连续输出可推进其位置。
    ///
    /// @author kongweiguang
    pub(crate) fn acknowledge_pending_scroll(
        &mut self,
        binding: &UiSessionBinding,
        gesture_id: u64,
        request_id: u64,
        min_snapshot_revision: u64,
    ) -> bool {
        if self.binding.as_ref() != Some(binding) || self.gesture_id != gesture_id {
            return false;
        }
        let Some(pending) = self.pending_scroll.as_mut() else {
            return false;
        };
        if pending.request_id != request_id {
            return false;
        }
        pending.min_snapshot_revision = Some(min_snapshot_revision);
        true
    }

    /// 滚动 offset 已确认或等待超时后，允许按当前权威快照继续映射选择端点。
    ///
    /// @author kongweiguang
    pub(crate) fn clear_pending_scroll(&mut self) {
        self.pending_scroll = None;
    }

    /// 事件闭包只取得与输入 handler 完整绑定吻合的 token，避免新鼠标事件接续旧 pane 的拖动。
    ///
    /// @author kongweiguang
    pub(crate) fn token_for_binding(
        &self,
        binding: &UiSessionBinding,
    ) -> Option<TerminalSelectionGestureToken> {
        (self.binding.as_ref() == Some(binding)).then(|| TerminalSelectionGestureToken {
            binding: binding.clone(),
            generation: self.generation,
            gesture_id: self.gesture_id,
        })
    }

    /// 指针进入不同 readonly view binding 时由持有层取消当前手势，Legacy API 不借此跨 pane 续接。
    ///
    /// @author kongweiguang
    pub(crate) fn belongs_to_binding(&self, binding: Option<&UiSessionBinding>) -> bool {
        self.binding.as_ref() == binding
    }

    /// 取消消息只暴露 Canvas 手势身份；legacy session-only 拖动没有 anchor token，不产生取消事件。
    ///
    /// @author kongweiguang
    pub(crate) fn canvas_cancellation_identity(&self) -> Option<(SessionId, u64)> {
        self.binding
            .as_ref()
            .map(|binding| (binding.session_id.clone(), self.gesture_id))
    }

    /// 生成与实时预览相同的绝对选择，确保 MouseUp 的最终回执与最后一帧一致。
    ///
    /// @author kongweiguang
    fn selection_for(&self, end: TerminalPoint) -> Option<TerminalSelection> {
        (self.start != end || self.selection_kind != TerminalSelectionKind::Character).then_some(
            TerminalSelection {
                start: self.start,
                end,
                kind: self.selection_kind,
            },
        )
    }

    /// 保留原有结束 API；调用方未核验 release 时不允许通过它触发点击清理。
    ///
    /// @author kongweiguang
    pub fn finish(
        self,
        session_id: &SessionId,
        end: TerminalPoint,
    ) -> Option<CompletedViewportSelectionDrag> {
        self.finish_after_release(session_id, end, false)
    }

    /// 结束时再核验同会话、无修饰 release 与零长度点击，保证一次手势只发清除或新选择之一。
    ///
    /// @author kongweiguang
    pub fn finish_after_release(
        self,
        session_id: &SessionId,
        end: TerminalPoint,
        plain_unmodified_release: bool,
    ) -> Option<CompletedViewportSelectionDrag> {
        if &self.session_id != session_id {
            return None;
        }

        let selection = self.selection_for(end);
        let clear_selection =
            self.clear_selection_on_click && plain_unmodified_release && selection.is_none();
        Some(CompletedViewportSelectionDrag {
            start: self.start,
            open_link_on_click: self.open_link_on_click,
            selection,
            clear_selection,
        })
    }
}

/// 一次拖动的稳定起点、点击链接意图、选择结果及已在 MouseUp 完成核验的清理决定。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletedViewportSelectionDrag {
    pub start: TerminalPoint,
    pub open_link_on_click: bool,
    pub selection: Option<TerminalSelection>,
    pub clear_selection: bool,
}

/// 复制与清除都明确绑定一个 session，防止在异步切 Tab 后读取另一份选择。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalSelectionEvent {
    Copy { session_id: SessionId },
    Clear { session_id: SessionId },
}

/// 复制状态栏只显示应用返回的选择摘要；剪贴板写入仍由 Root 和 SessionService 执行。
pub struct SelectionControls {
    session_id: Option<SessionId>,
    selected_chars: Option<usize>,
}

impl EventEmitter<TerminalSelectionEvent> for SelectionControls {}

impl SelectionControls {
    /// 以稳定 session 身份创建轻量控件，未收到真实 selection 时不提供复制入口。
    pub fn new(session_id: Option<SessionId>) -> Self {
        Self {
            session_id,
            selected_chars: None,
        }
    }

    /// 终端切换后丢弃旧选择提示，避免剪贴板控件看似作用于新会话却读取旧目标。
    pub fn set_session(&mut self, session_id: Option<SessionId>, cx: &mut Context<Self>) {
        if self.session_id != session_id {
            self.session_id = session_id;
            self.selected_chars = None;
            cx.notify();
        }
    }

    /// 只接受当前 session 的提取结果，None 表示 selection 已清除或没有可复制文本。
    pub fn set_selected_chars(
        &mut self,
        session_id: &SessionId,
        selected_chars: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.selected_chars = selected_chars.filter(|count| *count > 0);
        cx.notify();
    }
}

impl Render for SelectionControls {
    /// 有可复制内容时才显示快捷操作；剪贴板和异步读取仍留给宿主视图。
    /// Copy 紧邻字符数且明确命名所选终端文本，清除选择保留文字。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let selected_chars = self.selected_chars;
        let copy_session = self.session_id.clone();
        let clear_session = self.session_id.clone();
        div()
            .id("terminal-selection-controls")
            .flex()
            .items_center()
            .gap_1()
            .text_color(theme.muted)
            .when_some(
                selected_chars.filter(|_| self.session_id.is_some()),
                |bar, count| {
                    bar.child(div().text_size(px(11.)).child(crate::locale::text(
                        format!("已选 {count} 字符"),
                        format!("{count} characters selected"),
                    )))
                    .child(
                        icon_button(
                            "terminal-selection-copy",
                            gpui_kit::assets::IconName::Copy,
                            crate::locale::text("复制所选终端文本", "Copy selected terminal text"),
                        )
                        .with_size(gpui_kit::component::Size::Medium)
                        .on_click(cx.listener(
                            move |_, _: &ClickEvent, _, cx| {
                                if let Some(session_id) = copy_session.clone() {
                                    cx.emit(TerminalSelectionEvent::Copy { session_id });
                                }
                            },
                        )),
                    )
                    .child(
                        Button::new("terminal-selection-clear")
                            .compact()
                            .label(crate::locale::text("清除选择", "Clear selection"))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                if let Some(session_id) = clear_session.clone() {
                                    cx.emit(TerminalSelectionEvent::Clear { session_id });
                                }
                            })),
                    )
                },
            )
    }
}

/// 按 row.viewport_row 将画布 cell 映射到绝对行号，兼容稀疏增量快照且不重算字符宽度。
///
/// @author kongweiguang
pub fn selection_from_viewport_drag(
    rows: &[TerminalRow],
    columns: u16,
    start: ViewportCell,
    end: ViewportCell,
    kind: TerminalSelectionKind,
) -> Option<TerminalSelection> {
    let start = terminal_point_from_cell(rows, columns, start)?;
    let end = terminal_point_from_cell(rows, columns, end)?;
    Some(TerminalSelection { start, end, kind })
}

/// 将像素换算后的视口 cell 映射为引擎快照提供的绝对行号，拒绝缺失的稀疏增量行。
///
/// @author kongweiguang
pub fn terminal_point_from_cell(
    rows: &[TerminalRow],
    columns: u16,
    cell: ViewportCell,
) -> Option<TerminalPoint> {
    if columns == 0 {
        return None;
    }
    let row = rows.iter().find(|row| row.viewport_row == cell.row)?;
    Some(TerminalPoint {
        line: row.absolute_line,
        column: cell.column.min(columns.saturating_sub(1)),
    })
}

/// 从同一快照映射 endpoint、viewport 代次和行证据，避免异步选择把旧坐标配给新内容。
///
/// @author kongweiguang
pub(crate) fn terminal_point_and_viewport_from_cell(
    snapshot: &RenderSnapshot,
    cell: ViewportCell,
) -> Option<(TerminalPoint, SelectionViewport)> {
    let point = terminal_point_from_cell(&snapshot.rows, snapshot.size.columns(), cell)?;
    // absolute_line 随 viewport offset 平移；还原该快照中的 viewport 行后，才能把证据
    // 与同一份渲染内容配对，供 engine 在异步执行时识别被覆盖或裁剪的旧端点。
    let endpoint_row_witness = i64::try_from(snapshot.scrollback_offset)
        .ok()
        .and_then(|offset| point.line.checked_add(offset))
        .and_then(|row| u16::try_from(row).ok())
        .and_then(|viewport_row| {
            snapshot
                .rows
                .iter()
                .find(|row| row.viewport_row == viewport_row && row.absolute_line == point.line)
        })
        .map(TerminalRow::witness);
    Some((
        point,
        SelectionViewport {
            size: snapshot.size,
            scrollback_offset: snapshot.scrollback_offset,
            scroll_origin: snapshot.scroll_origin,
            grid_epoch: snapshot.grid_epoch,
            endpoint_row_witness,
        },
    ))
}

#[cfg(test)]
#[path = "../../../tests/unit/terminal_selection_drag.rs"]
mod tests;
