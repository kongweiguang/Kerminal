//! 每个终端 pane 独占的文本输入代理，所有回调都携带创建时的不可变会话绑定。
//!
//! @author kongweiguang

use std::ops::Range;

use gpui_kit::{
    Bounds, ClipboardItem, Context, EntityInputHandler, FocusHandle, Pixels, Point, UTF16Selection,
    Window, bounds, point, px, size,
};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::identity::SessionId;
use kerminal_terminal::{input::TerminalInputEvent, snapshot::TerminalCursor};

use crate::{
    root::terminal_ime::{
        ImeCompositionBinding, ImeInputDecision, ImeTextSource, TerminalImeState,
    },
    terminal_metrics::TerminalMetrics,
};

/// 文本回调额外标记来源，让 Root 只对普通按键文本执行 Kitty 重复提交去重。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneTextOrigin {
    Plain,
    Composition { generation: u64 },
}

/// 复用隔离 resize 验收开关，避免生产常驻输入日志或记录提交正文。
///
/// @author kongweiguang
pub(super) fn ime_diagnostics_enabled() -> bool {
    std::env::var_os("KERMINAL_TERMINAL_RESIZE_DIAGNOSTICS").is_some_and(|value| value == "1")
}

/// 输入事件始终带着创建 handler 时的完整 owner 和 session generation。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneTerminalInputEvent {
    pub binding: UiSessionBinding,
    pub event: TerminalInputEvent,
    /// 只有 `Text` 回调有来源；Composition 不得被普通按键重复去重逻辑吞掉。
    pub text_origin: Option<PaneTextOrigin>,
}

/// 供对应 pane 的终端画布渲染暂存组合文本；选区单位明确为 UTF-8 字节。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneImePreview {
    pub text: String,
    pub selection_bytes: Range<usize>,
}

/// 组合开始时通知 Root 回到底部，避免历史 viewport 的光标坐标参与候选定位。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneTerminalViewportEvent {
    pub binding: UiSessionBinding,
}

/// 输入法候选定位只使用这一 pane 的光标与字体度量，不依赖全局当前焦点。
#[derive(Clone, Debug)]
struct PaneImeProjection {
    cursor: TerminalCursor,
    scrollback_offset: usize,
    metrics: TerminalMetrics,
}

impl PaneImeProjection {
    /// 历史行的 cursor 仍是 viewport 坐标，但不是 IME 锚点；只允许 offset 归零的投影。
    ///
    /// @author kongweiguang
    fn allows_candidate_position(&self) -> bool {
        self.scrollback_offset == 0
    }
}

/// 组合缓冲与生命周期状态只属于一个固定的 session owner/generation。
struct PaneInputState {
    binding: UiSessionBinding,
    ime: TerminalImeState,
    composition: Option<ImeCompositionBinding>,
    buffer: String,
    selection_bytes: Range<usize>,
    active: bool,
    focus_loss_pending: bool,
    plain_keydown_after_focus_loss: Option<String>,
}

impl PaneInputState {
    /// 创建独立输入状态；Agent 与 UI owner 都保留同一 typed binding，不接受后台 owner。
    fn new(binding: UiSessionBinding) -> Self {
        let ime = TerminalImeState::new(binding.clone());
        Self {
            binding,
            ime,
            composition: None,
            buffer: String::new(),
            selection_bytes: 0..0,
            active: true,
            focus_loss_pending: false,
            plain_keydown_after_focus_loss: None,
        }
    }

    /// 更新一次预编辑内容并保留平台给出的 UTF-16 选区在本地缓冲中的精确范围。
    ///
    /// @author kongweiguang
    fn update_composition(
        &mut self,
        text: &str,
        selected_range_utf16: Option<Range<usize>>,
    ) -> Option<u64> {
        if !self.active {
            return None;
        }

        let token = self.ime.begin_or_update_composition();
        let generation = token.generation();
        self.composition = Some(token);
        self.focus_loss_pending = false;
        self.plain_keydown_after_focus_loss = None;
        self.buffer.clear();
        self.buffer.push_str(text);
        self.selection_bytes = selected_range_utf16
            .map(|range| utf16_range_to_bytes(&self.buffer, range))
            .unwrap_or_else(|| self.buffer.len()..self.buffer.len());
        Some(generation)
    }

    /// 结束当前预编辑；保留 binding 以便后续的迟到回调仍可识别其原始目标。
    ///
    /// @author kongweiguang
    fn cancel_composition(&mut self) {
        if !self.active {
            return;
        }
        self.ime.cancel_for_close();
        self.composition = None;
        self.buffer.clear();
        self.selection_bytes = 0..0;
    }

    /// 焦点离开时撤销未提交组合；重获焦点后 Plain 回调须匹配新按键，避免旧提交冒充新文本。
    ///
    /// @author kongweiguang
    fn lose_focus(&mut self) {
        if !self.active {
            return;
        }
        self.ime.cancel_for_focus_loss();
        self.composition = None;
        self.buffer.clear();
        self.selection_bytes = 0..0;
        self.focus_loss_pending = true;
        self.plain_keydown_after_focus_loss = None;
    }

    /// 记录失焦后、真实聚焦 pane 收到的按键文本，供 GPUI 的无 token Plain 回调核对。
    ///
    /// @author kongweiguang
    fn note_plain_keydown_after_focus_loss(&mut self, text: Option<&str>) {
        if !self.focus_loss_pending {
            return;
        }
        self.plain_keydown_after_focus_loss =
            text.filter(|text| !text.is_empty()).map(str::to_owned);
    }

    /// 只接受该 handler 自己的 plain/IME commit；失焦后的无令牌回调还须匹配新物理按键。
    ///
    /// @author kongweiguang
    fn commit_text(&mut self, text: &str) -> Option<(String, ImeTextSource)> {
        if !self.active {
            return None;
        }

        if self.focus_loss_pending && self.composition.is_none() {
            let matches_fresh_keydown =
                self.plain_keydown_after_focus_loss.take().as_deref() == Some(text);
            if !matches_fresh_keydown {
                return None;
            }
            self.focus_loss_pending = false;
        }

        let decision = match self.composition.as_ref() {
            Some(token) => self
                .ime
                .commit_composition(Some(token), Some(&self.binding), text),
            None => self.ime.accept_plain_text(Some(&self.binding), text),
        };
        self.composition = None;
        self.buffer.clear();
        self.selection_bytes = 0..0;

        match decision {
            ImeInputDecision::Accepted(bound) => Some((bound.text, bound.source)),
            ImeInputDecision::Stale { .. }
            | ImeInputDecision::Unbound
            | ImeInputDecision::CompositionActive => None,
        }
    }

    /// 永久关闭旧 handler，避免平台仍持有的 EntityInputHandler 触达新 generation。
    ///
    /// @author kongweiguang
    fn invalidate(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.ime.cancel_for_close();
        self.composition = None;
        self.buffer.clear();
        self.selection_bytes = 0..0;
    }

    /// 只有有效 handler 且不在 IME 组合期间才允许附带 Kitty associated text。
    ///
    /// @author kongweiguang
    fn can_send_kitty_associated_text(&self) -> bool {
        self.active && self.composition.is_none()
    }

    /// 只在状态确实需要清理时响应焦点变化，避免无关失焦导致重复重绘。
    ///
    /// @author kongweiguang
    fn needs_focus_loss_cancellation(&self) -> bool {
        self.active
            && (self.composition.is_some()
                || !self.focus_loss_pending
                || self.plain_keydown_after_focus_loss.is_some())
    }
}

/// 终端文本输入 Entity；Root 按每个 pane 持有一份并只在完整 binding 相等时复用。
pub struct PaneTerminalInputHandler {
    state: PaneInputState,
    focus_handle: FocusHandle,
    projection: Option<PaneImeProjection>,
}

impl PaneTerminalInputHandler {
    /// 绑定独立 FocusHandle 与会话代次；重连时 Root 必须创建新 Entity 和新 FocusHandle。
    pub fn new(binding: UiSessionBinding, focus_handle: FocusHandle) -> Self {
        Self {
            state: PaneInputState::new(binding),
            focus_handle,
            projection: None,
        }
    }

    /// 返回创建时固定的完整 binding，供 Root 在订阅和输入队列边界再次比较。
    pub fn binding(&self) -> &UiSessionBinding {
        &self.state.binding
    }

    /// 返回该 pane 私有的 FocusHandle；焦点切换不改变输入目标的 session binding。
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 返回可绘制的预编辑文本和 UTF-8 选区；历史视口未确认回底前隐藏预编辑，避免画在错误网格位置。
    ///
    /// @author kongweiguang
    pub fn ime_preview(&self) -> Option<PaneImePreview> {
        (self.is_composing()
            && self
                .projection
                .as_ref()
                .is_some_and(PaneImeProjection::allows_candidate_position))
        .then(|| PaneImePreview {
            text: self.state.buffer.clone(),
            selection_bytes: self.state.selection_bytes.clone(),
        })
    }

    /// 反映 handler 的真实 marked composition 状态，不随历史视口的预览隐藏而改变。
    ///
    /// @author kongweiguang
    pub fn is_composing(&self) -> bool {
        self.state.active && self.state.composition.is_some()
    }

    /// 供同 binding 的键盘路径判断是否可合并 Kitty 关联文本，不猜测系统 IME 状态。
    pub fn can_send_kitty_associated_text(&self) -> bool {
        self.state.can_send_kitty_associated_text()
    }

    /// 告知 Root 是否有未提交组合或待核对的失焦后文本候选需要撤销。
    ///
    /// @author kongweiguang
    pub fn needs_focus_loss_cancellation(&self) -> bool {
        self.state.needs_focus_loss_cancellation()
    }

    /// 只允许当前被聚焦 handler 记录 Plain 候选；Root 先完成当前 pane/binding 校验。
    ///
    /// @author kongweiguang
    pub fn note_plain_keydown_after_focus_loss(&mut self, text: Option<&str>) {
        self.state.note_plain_keydown_after_focus_loss(text);
    }

    /// 焦点离开仅取消本地组合与迟到提交资格，不永久失效 handler 或其 session FIFO。
    ///
    /// @author kongweiguang
    pub fn cancel_for_focus_loss(&mut self, cx: &mut Context<Self>) {
        self.state.lose_focus();
        cx.notify();
    }

    /// 仅接受匹配 binding 与 SessionId 的快照投影，并保存 offset 供 IME 拒绝历史页坐标。
    ///
    /// @author kongweiguang
    pub fn update_projection(
        &mut self,
        binding: &UiSessionBinding,
        snapshot_session_id: &SessionId,
        cursor: Option<TerminalCursor>,
        scrollback_offset: usize,
        metrics: TerminalMetrics,
    ) -> bool {
        if !projection_matches_binding(
            &self.state.binding,
            binding,
            snapshot_session_id,
            self.state.active,
        ) {
            self.projection = None;
            return false;
        }

        self.projection = cursor.map(|cursor| PaneImeProjection {
            cursor,
            scrollback_offset,
            metrics,
        });
        true
    }

    /// 关闭 pane 或更换 session generation 前撤销预编辑并永久拒绝旧实体的后续输入。
    pub fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.state.invalidate();
        self.projection = None;
        cx.notify();
    }

    /// 由 Root 的 pane-scoped 键盘回调投递按键，事件始终使用实体固定的 binding。
    pub fn emit_input_event(&mut self, event: TerminalInputEvent, cx: &mut Context<Self>) -> bool {
        if !self.state.active {
            return false;
        }
        cx.emit(PaneTerminalInputEvent {
            binding: self.state.binding.clone(),
            event,
            text_origin: None,
        });
        true
    }

    /// 仅在原 pane 仍实际持有窗口焦点时提交文本，防止迟到的 Plain 回调复活已取消组合。
    ///
    /// @author kongweiguang
    fn emit_text_commit(&mut self, text: &str, window: &Window, cx: &mut Context<Self>) {
        if ime_diagnostics_enabled() {
            tracing::info!(
                event = "TERMINAL_IME_DIAGNOSTIC",
                phase = 1_u64,
                count = text.encode_utf16().count() as u64,
                ime_focus_owned = u64::from(self.focus_handle.is_focused(window))
            );
        }
        if !self.focus_handle.is_focused(window) {
            if ime_diagnostics_enabled() {
                tracing::info!(
                    event = "TERMINAL_IME_DIAGNOSTIC",
                    phase = 2_u64,
                    ime_reason = 1_u64
                );
            }
            self.state.lose_focus();
            cx.notify();
            return;
        }
        let committed = self.state.commit_text(text);
        if ime_diagnostics_enabled() {
            tracing::info!(
                event = "TERMINAL_IME_DIAGNOSTIC",
                phase = 3_u64,
                ime_accepted = u64::from(committed.is_some())
            );
        }
        if let Some((text, source)) = committed {
            let text_origin = match source {
                ImeTextSource::Plain => PaneTextOrigin::Plain,
                ImeTextSource::Composition { generation } => {
                    PaneTextOrigin::Composition { generation }
                }
            };
            cx.emit(PaneTerminalInputEvent {
                binding: self.state.binding.clone(),
                event: TerminalInputEvent::Text(text),
                text_origin: Some(text_origin),
            });
        }
        cx.notify();
    }
}

impl gpui_kit::EventEmitter<PaneTerminalInputEvent> for PaneTerminalInputHandler {}
impl gpui_kit::EventEmitter<PaneTerminalViewportEvent> for PaneTerminalInputHandler {}

impl EntityInputHandler for PaneTerminalInputHandler {
    /// 按 GPUI UTF-16 索引读取本 handler 的预编辑缓冲并回传有效边界。
    ///
    /// @author kongweiguang
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if !self.accepts_focused_text_input(window) {
            return None;
        }
        let byte_range = utf16_range_to_bytes(&self.state.buffer, range);
        *adjusted_range = Some(bytes_to_utf16_range(&self.state.buffer, byte_range.clone()));
        self.state.buffer.get(byte_range).map(str::to_owned)
    }

    /// 将输入法选区限定在本地 composition buffer，不影响终端网格选区。
    ///
    /// @author kongweiguang
    fn selected_text_range(
        &mut self,
        _: bool,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.accepts_focused_text_input(window)
            .then(|| UTF16Selection {
                range: bytes_to_utf16_range(&self.state.buffer, self.state.selection_bytes.clone()),
                reversed: false,
            })
    }

    /// 仅在有效 handler 有组合文本时报告 marked range。
    ///
    /// @author kongweiguang
    fn marked_text_range(
        &self,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        (self.accepts_focused_text_input(window) && !self.state.buffer.is_empty())
            .then(|| 0..self.state.buffer.encode_utf16().count())
    }

    /// 取消当前 composition，但保留此 handler 的固定 binding 以拒绝跨 pane 重定向。
    ///
    /// @author kongweiguang
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if ime_diagnostics_enabled() {
            tracing::info!(
                event = "TERMINAL_IME_DIAGNOSTIC",
                phase = 4_u64,
                count = self.state.buffer.encode_utf16().count() as u64
            );
        }
        self.state.cancel_composition();
        cx.notify();
    }

    /// 将普通文本或 IME commit 作为 Text 事件发给 Root 的同会话 FIFO。
    ///
    /// @author kongweiguang
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.emit_text_commit(text, window, cx);
    }

    /// 只更新该 pane 的预编辑投影；组合初次出现时请求 Root 按固定 binding 回底，commit 前不写入字符。
    ///
    /// @author kongweiguang
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ime_diagnostics_enabled() {
            tracing::info!(
                event = "TERMINAL_IME_DIAGNOSTIC",
                phase = 5_u64,
                count = new_text.encode_utf16().count() as u64,
                ime_focus_owned = u64::from(self.focus_handle.is_focused(window))
            );
        }
        if !self.accepts_focused_text_input(window) {
            self.state.lose_focus();
            cx.notify();
            return;
        }
        let composition_started = self.state.composition.is_none();
        if self
            .state
            .update_composition(new_text, new_selected_range)
            .is_some()
        {
            if composition_started {
                cx.emit(PaneTerminalViewportEvent {
                    binding: self.state.binding.clone(),
                });
            }
            cx.notify();
        }
    }

    /// 按该 pane 最新且处于底部的 cursor projection 定位候选窗；历史页缺少正确锚点时返回 None。
    ///
    /// @author kongweiguang
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if !self.accepts_focused_text_input(window) {
            return None;
        }
        let projection = self.projection.as_ref()?;
        if !projection.allows_candidate_position() {
            return None;
        }
        let mut style = window.text_style();
        style.font_family = projection.metrics.font_family.clone();
        style.font_size = px(projection.metrics.font_size).into();
        style.line_height = px(projection.metrics.line_height).into();
        style.font_weight = crate::terminal_metrics::font_weight(projection.metrics.font_weight);

        let byte_end = utf16_range_to_bytes(&self.state.buffer, 0..range_utf16.start).end;
        let prefix = self.state.buffer.get(..byte_end)?;
        let prefix_width = if prefix.is_empty() {
            px(0.0)
        } else {
            window
                .text_system()
                .shape_line(
                    prefix.into(),
                    px(projection.metrics.font_size),
                    &[style.to_run(prefix.len())],
                    None,
                )
                .width()
        };
        let cell_width = projection.metrics.cell_width(window);
        let cell_origin = projection.metrics.cell_origin(
            element_bounds.origin,
            projection.cursor.row,
            projection.cursor.column,
            cell_width,
        );
        let origin = point(cell_origin.x + prefix_width, cell_origin.y);
        Some(bounds(
            origin,
            size(cell_width, px(projection.metrics.line_height)),
        ))
    }

    /// 终端没有普通文本点位映射；合法 pane 的平台查询统一返回 composition 起点。
    ///
    /// @author kongweiguang
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.accepts_focused_text_input(window).then_some(0)
    }

    /// 更新本地组合选区，不把 UTF-16 的平台坐标泄漏到终端网格选择。
    ///
    /// @author kongweiguang
    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        if self.accepts_focused_text_input(window) {
            self.state.selection_bytes = utf16_range_to_bytes(&self.state.buffer, range_utf16);
        }
    }

    /// 报告当前 pane 缓冲区长度；失效旧 handler 不向平台暴露新的输入范围。
    ///
    /// @author kongweiguang
    fn text_length_utf16(&mut self, window: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        self.accepts_focused_text_input(window)
            .then(|| self.state.buffer.encode_utf16().count())
    }

    /// 失效时关闭平台文本输入，避免旧 FocusId 仍接受输入法提交。
    ///
    /// @author kongweiguang
    fn accepts_text_input(&self, window: &mut Window, _: &mut Context<Self>) -> bool {
        self.accepts_focused_text_input(window)
    }

    /// 将系统剪贴板文本作为 bracketed-paste 事件发送到固定会话。
    ///
    /// @author kongweiguang
    fn paste(&mut self, item: ClipboardItem, window: &mut Window, cx: &mut Context<Self>) {
        if !self.accepts_focused_text_input(window) {
            self.state.lose_focus();
            cx.notify();
            return;
        }
        let Some(text) = item.text() else {
            return;
        };
        self.emit_input_event(TerminalInputEvent::Paste(text), cx);
    }
}

impl PaneTerminalInputHandler {
    /// 平台文本接口只对当前 FocusHandle 生效，旧 handler 即使仍注册也不得接收失焦回调。
    ///
    /// @author kongweiguang
    fn accepts_focused_text_input(&self, window: &Window) -> bool {
        self.state.active && self.focus_handle.is_focused(window)
    }
}

/// 按 Root 当前活动 binding、已注册 handler 和实际焦点三重校验终端键盘路由。
///
/// @author kongweiguang
pub(super) fn input_binding_is_focused(
    expected: &UiSessionBinding,
    registered: &UiSessionBinding,
    focused: Option<&UiSessionBinding>,
    focus_handle_is_focused: bool,
) -> bool {
    expected == registered && focused == Some(expected) && focus_handle_is_focused
}

/// 将 UTF-16 输入范围裁剪到字符串中的 UTF-8 字符边界。
///
/// @author kongweiguang
fn utf16_range_to_bytes(text: &str, range: Range<usize>) -> Range<usize> {
    let start = byte_offset_for_utf16(text, range.start);
    let end = byte_offset_for_utf16(text, range.end);
    start.min(end)..start.max(end)
}

/// 找到不切分代理对的 UTF-8 字节偏移，越界 UTF-16 偏移安全夹到文本末尾。
///
/// @author kongweiguang
fn byte_offset_for_utf16(text: &str, target: usize) -> usize {
    let mut utf16_offset = 0;
    for (byte_offset, character) in text.char_indices() {
        if utf16_offset >= target {
            return byte_offset;
        }
        let next_offset = utf16_offset + character.len_utf16();
        if target < next_offset {
            return byte_offset;
        }
        utf16_offset = next_offset;
    }
    text.len()
}

/// 将 UTF-8 字节选区转成平台 UTF-16 坐标，并将偏移夹回合法字符边界。
///
/// @author kongweiguang
fn bytes_to_utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = byte_boundary_floor(text, range.start);
    let end = byte_boundary_floor(text, range.end);
    let start_utf16 = text[..start].encode_utf16().count();
    let end_utf16 = text[..end].encode_utf16().count();
    start_utf16.min(end_utf16)..start_utf16.max(end_utf16)
}

/// 将任意字节偏移向前收敛到合法 UTF-8 边界，避免选区查询切开多字节字符。
///
/// @author kongweiguang
fn byte_boundary_floor(text: &str, offset: usize) -> usize {
    let mut boundary = offset.min(text.len());
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

/// 仅当 view、runtime binding 与快照会话同时吻合时允许更新候选窗坐标。
///
/// @author kongweiguang
fn projection_matches_binding(
    handler_binding: &UiSessionBinding,
    projected_binding: &UiSessionBinding,
    snapshot_session_id: &SessionId,
    handler_is_active: bool,
) -> bool {
    handler_is_active
        && handler_binding == projected_binding
        && handler_binding.session_id == *snapshot_session_id
}

#[cfg(test)]
#[path = "../../tests/unit/pane_input.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/root_pane_input_ime_viewport.rs"]
mod ime_viewport_tests;
