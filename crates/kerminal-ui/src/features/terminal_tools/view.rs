//! 终端工具右栏的 GPUI 组合视图；业务动作统一以类型化事件交回 Root。
//!
//! @author kongweiguang

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, IntoElement, Render,
    Subscription, Window, WindowAppearance, component::button::Button, div, prelude::*,
};
use kerminal_application::preferences::{TerminalKeywordHighlightSettings, ThemeMode};
use kerminal_application::sessions::{UiSessionBinding, logging::SessionLogState};
use kerminal_domain::identity::{AgentSessionId, SessionId};
use kerminal_terminal::snapshot::{
    RenderSnapshot, SelectionViewport, TerminalPoint, TerminalRow, TerminalSearchRequest,
    TerminalSearchResult, TerminalSelection, TerminalSelectionKind,
};

use crate::terminal_canvas::{
    TerminalCellOverlay, TerminalOverlayKind, TerminalOverlayStyle, scrollbar_drag_offset,
};
use crate::theme::ThemeTokens;

use super::selection::{
    CompletedViewportSelectionDrag, TerminalSelectionGestureFrame, TerminalSelectionGestureToken,
    ViewportCell, ViewportSelectionDrag,
};
use super::{
    BroadcastEvent, BroadcastTargetPicker, BroadcastTargetProjection, CommandBlockAction,
    CommandBlockProjection, CommandBlockTimeline, SafeTerminalLink, SelectionControls,
    TerminalLogControls, TerminalLogEvent, TerminalSearchEvent, TerminalSearchToolbar,
    TerminalSelectionEvent, selection_from_viewport_drag, validate_terminal_link,
};

/// 终端 UI 统一发布用户意图，由 Root 验证会话并调用 Application 用例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalToolsEvent {
    Search {
        session_id: SessionId,
        request_id: u64,
        request: TerminalSearchRequest,
        navigate: bool,
        safe_top_rows: u16,
    },
    SelectMatch {
        session_id: SessionId,
        request_id: u64,
        request: TerminalSearchRequest,
        selected_match: usize,
        safe_top_rows: u16,
    },
    RefreshSearch {
        session_id: SessionId,
        request_id: u64,
        request: TerminalSearchRequest,
        selected_match: Option<usize>,
    },
    ClearSearch {
        session_id: SessionId,
    },
    CloseSearch {
        session_id: SessionId,
    },
    SetSelection {
        session_id: SessionId,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
        started: bool,
        finalized: bool,
        expected_revision: Option<u64>,
        viewport: Option<SelectionViewport>,
    },
    CancelSelectionGesture {
        session_id: SessionId,
        gesture_id: u64,
    },
    CopySelection {
        session_id: SessionId,
    },
    ClearSelection {
        session_id: SessionId,
    },
    OpenLink {
        link: SafeTerminalLink,
    },
    BroadcastStart {
        session_ids: Vec<SessionId>,
    },
    BroadcastStop,
    BroadcastCancel,
    LogStart {
        session_id: SessionId,
    },
    LogStop {
        session_id: SessionId,
    },
    LogRefresh {
        session_id: SessionId,
    },
    CommandBlockAction {
        session_id: SessionId,
        block_id: kerminal_application::sessions::CommandBlockId,
        capture_revision: Option<u64>,
        action: CommandBlockAction,
    },
}

/// Root 保持业务服务与 session registry；本视图只组合工具控件和只读投影。
pub struct TerminalToolsView {
    session_id: Option<SessionId>,
    search: Entity<TerminalSearchToolbar>,
    selection: Entity<SelectionControls>,
    broadcast: Entity<BroadcastTargetPicker>,
    logs: Entity<TerminalLogControls>,
    command_blocks: Entity<CommandBlockTimeline>,
    keyword_overlays: super::KeywordOverlayProjector,
    search_result: Option<TerminalSearchResult>,
    search_result_revision: Option<u64>,
    search_overlay_cache: RefCell<Option<SearchOverlayCache>>,
    selection_drag: Option<ViewportSelectionDrag>,
    selection_drag_generation: u64,
    selection_autoscroll_last: Option<Instant>,
    selection_autoscroll_scheduled: Option<(UiSessionBinding, u64)>,
    scrollbar_drag: Option<TerminalScrollbarDrag>,
    active_search_request_id: u64,
    search_visible: bool,
    broadcast_visible: bool,
    command_block_rail_visible: bool,
    log_available: bool,
    keyword_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// 保存滚动条开始时的几何和完整 owner，使重绘后的 move 仍映射到同一条视口轨道。
#[derive(Clone, Debug, PartialEq)]
struct TerminalScrollbarDrag {
    binding: UiSessionBinding,
    initial_offset: usize,
    max_offset: usize,
    pointer_start_y: f32,
    thumb_travel: f32,
}

/// 只缓存一个可见快照的搜索投影，避免同一帧反复比较行见证或分配高亮。
struct SearchOverlayCache {
    snapshot: RenderSnapshot,
    overlays: Arc<[TerminalCellOverlay]>,
}

impl SearchOverlayCache {
    /// 行 Arc 身份区分同 revision 的滚动快照，拓扑水位防止跨网格复用坐标。
    fn matches(&self, snapshot: &RenderSnapshot) -> bool {
        self.snapshot.session_id == snapshot.session_id
            && self.snapshot.revision == snapshot.revision
            && self.snapshot.size == snapshot.size
            && self.snapshot.grid_epoch == snapshot.grid_epoch
            && self.snapshot.scroll_origin == snapshot.scroll_origin
            && self.snapshot.history_size == snapshot.history_size
            && self.snapshot.scrollback_offset == snapshot.scrollback_offset
            && Arc::ptr_eq(&self.snapshot.rows, &snapshot.rows)
    }
}

impl EventEmitter<TerminalToolsEvent> for TerminalToolsView {}

impl TerminalToolsView {
    /// 注入当前目标会话并为子控件建立事件转发，保持 GPUI 子状态与 Root 业务状态分层。
    pub fn new(session_id: Option<SessionId>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TerminalSearchToolbar::new(session_id.clone(), window, cx));
        let selection = cx.new(|_| SelectionControls::new(session_id.clone()));
        let broadcast = cx.new(|_| BroadcastTargetPicker::new(Vec::new()));
        let logs = cx.new(|_| TerminalLogControls::new(session_id.clone()));
        let command_blocks = cx.new(|cx| {
            let mut timeline = CommandBlockTimeline::new();
            timeline.set_session(session_id.clone(), cx);
            timeline
        });

        let search_subscription = cx.subscribe(&search, |view, _, event, cx| match event.clone() {
            TerminalSearchEvent::Search {
                session_id,
                request_id,
                request,
                navigate,
                safe_top_rows,
            } => {
                if view.session_id.as_ref() != Some(&session_id)
                    || !view.search_visible
                    || view.active_search_request_id.wrapping_add(1) != request_id
                {
                    return;
                }
                view.active_search_request_id = request_id;
                view.clear_search_result();
                cx.emit(TerminalToolsEvent::Search {
                    session_id,
                    request_id,
                    request,
                    navigate,
                    safe_top_rows,
                });
                cx.notify();
            }
            TerminalSearchEvent::SelectMatch {
                session_id,
                request_id,
                request,
                selected_match,
                safe_top_rows,
            } => {
                if view.session_id.as_ref() != Some(&session_id)
                    || view.active_search_request_id.wrapping_add(1) != request_id
                {
                    return;
                }
                view.active_search_request_id = request_id;
                view.clear_search_result();
                cx.emit(TerminalToolsEvent::SelectMatch {
                    session_id,
                    request_id,
                    request,
                    selected_match,
                    safe_top_rows,
                });
                cx.notify();
            }
            TerminalSearchEvent::Refresh {
                session_id,
                request_id,
                request,
                selected_match,
            } => {
                if view.session_id.as_ref() != Some(&session_id)
                    || !view.search_visible
                    || view.active_search_request_id.wrapping_add(1) != request_id
                {
                    return;
                }
                view.active_search_request_id = request_id;
                view.clear_search_result();
                cx.emit(TerminalToolsEvent::RefreshSearch {
                    session_id,
                    request_id,
                    request,
                    selected_match,
                });
                cx.notify();
            }
            TerminalSearchEvent::Clear { session_id } => {
                if view.session_id.as_ref() != Some(&session_id) {
                    return;
                }
                view.active_search_request_id = view.active_search_request_id.wrapping_add(1);
                view.clear_search_result();
                cx.emit(TerminalToolsEvent::ClearSearch { session_id });
                cx.notify();
            }
            TerminalSearchEvent::Close { session_id } => {
                if view.session_id.as_ref() != Some(&session_id) {
                    return;
                }
                view.active_search_request_id = view.active_search_request_id.wrapping_add(1);
                view.search_visible = false;
                view.clear_search_result();
                cx.emit(TerminalToolsEvent::CloseSearch { session_id });
                cx.notify();
            }
        });
        let selection_subscription =
            cx.subscribe(&selection, |_, _, event, cx| match event.clone() {
                TerminalSelectionEvent::Copy { session_id } => {
                    cx.emit(TerminalToolsEvent::CopySelection { session_id });
                }
                TerminalSelectionEvent::Clear { session_id } => {
                    cx.emit(TerminalToolsEvent::ClearSelection { session_id });
                }
            });
        let broadcast_subscription =
            cx.subscribe(&broadcast, |view, _, event, cx| match event.clone() {
                BroadcastEvent::Start { session_ids } => {
                    cx.emit(TerminalToolsEvent::BroadcastStart { session_ids });
                }
                BroadcastEvent::Stop => cx.emit(TerminalToolsEvent::BroadcastStop),
                BroadcastEvent::Cancel => {
                    view.broadcast_visible = false;
                    cx.emit(TerminalToolsEvent::BroadcastCancel);
                    cx.notify();
                }
            });
        let logs_subscription = cx.subscribe(&logs, |_, _, event, cx| match event.clone() {
            TerminalLogEvent::Start { session_id } => {
                cx.emit(TerminalToolsEvent::LogStart { session_id });
            }
            TerminalLogEvent::Stop { session_id } => {
                cx.emit(TerminalToolsEvent::LogStop { session_id });
            }
            TerminalLogEvent::Refresh { session_id } => {
                cx.emit(TerminalToolsEvent::LogRefresh { session_id });
            }
        });
        let command_blocks_subscription = cx.subscribe(&command_blocks, |view, _, event, cx| {
            if view.session_id.as_ref() != Some(&event.session_id) {
                return;
            }
            cx.emit(TerminalToolsEvent::CommandBlockAction {
                session_id: event.session_id.clone(),
                block_id: event.block_id,
                capture_revision: event.capture_revision,
                action: event.action,
            });
        });

        Self {
            session_id,
            search,
            selection,
            broadcast,
            logs,
            command_blocks,
            keyword_overlays: super::KeywordOverlayProjector::default(),
            search_result: None,
            search_result_revision: None,
            search_overlay_cache: RefCell::new(None),
            selection_drag: None,
            selection_drag_generation: 0,
            selection_autoscroll_last: None,
            selection_autoscroll_scheduled: None,
            scrollbar_drag: None,
            active_search_request_id: 0,
            search_visible: false,
            broadcast_visible: false,
            command_block_rail_visible: false,
            log_available: false,
            keyword_error: None,
            _subscriptions: vec![
                search_subscription,
                selection_subscription,
                broadcast_subscription,
                logs_subscription,
                command_blocks_subscription,
            ],
        }
    }

    /// 目标切换后重置会话与搜索坐标，并从 toolbar 同步代次以承接尚未回投的挂起事件。
    /// Clear 可能因旧 session 在异步订阅中被丢弃，因此父 view 不自行猜测子实体推进了几次。
    ///
    /// @author kongweiguang
    pub fn set_session(
        &mut self,
        session_id: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_id == session_id {
            return;
        }
        self.session_id = session_id.clone();
        self.selection_drag = None;
        self.selection_drag_generation = self.selection_drag_generation.wrapping_add(1);
        self.selection_autoscroll_last = None;
        self.selection_autoscroll_scheduled = None;
        self.scrollbar_drag = None;
        self.clear_search_result();
        self.active_search_request_id = self.search.update(cx, |search, cx| {
            search.set_session(session_id.clone(), window, cx);
            search.current_request_id()
        });
        self.selection.update(cx, |selection, cx| {
            selection.set_session(session_id.clone(), cx);
        });
        self.logs.update(cx, |logs, cx| {
            logs.set_session(session_id.clone(), cx);
        });
        self.command_blocks
            .update(cx, |timeline, cx| timeline.set_session(session_id, cx));
        self.search_visible = false;
        cx.notify();
    }

    /// 将语言切换投影到已保留的搜索输入，避免重建工具面板而丢失用户的查询状态。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.refresh_locale(window, cx));
    }

    /// 只为构造时绑定的 session 打开并聚焦搜索，拒绝将旧 action 转投到另一个终端。
    ///
    /// @author kongweiguang
    pub fn open_search_for(
        &mut self,
        session_id: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session_id.as_ref() != Some(session_id) {
            return false;
        }
        let changed = !self.search_visible;
        self.search_visible = true;
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
        if changed {
            cx.notify();
        }
        true
    }

    /// 只暴露当前 pane 已打开且仍绑定有效会话的搜索控件，供中心终端绘制浮层。
    ///
    /// @author kongweiguang
    pub(crate) fn search_overlay(&self) -> Option<Entity<TerminalSearchToolbar>> {
        (self.search_visible && self.session_id.is_some()).then(|| self.search.clone())
    }

    /// 只让仍打开的同 session 搜索栏重查已提交查询，尺寸/主题刷新不会恢复隐藏搜索。
    ///
    /// @author kongweiguang
    pub(crate) fn refresh_search_after_view_revision(
        &mut self,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session_id.as_ref() != Some(session_id) || !self.search_visible {
            return false;
        }
        self.search
            .update(cx, |search, cx| search.refresh_after_view_revision(cx))
    }

    /// 结果与同次扫描捕获的行见证一起保存；请求身份和坐标投影分别阻止迟到与错位。
    ///
    /// @author kongweiguang
    pub fn set_search_result(
        &mut self,
        session_id: &SessionId,
        request_id: u64,
        result: TerminalSearchResult,
        revision: u64,
        reveal_safe_margin_reached: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id)
            || self.active_search_request_id != request_id
        {
            return;
        }
        self.clear_search_result();
        self.search_result = Some(result.clone());
        self.search_result_revision = Some(revision);
        self.search.update(cx, |search, cx| {
            search.set_result(
                session_id,
                request_id,
                result,
                reveal_safe_margin_reached,
                cx,
            );
        });
    }

    /// 隐藏或失效的 pane 取消搜索生命周期并清除坐标；恢复时只重查仍打开的查询。
    ///
    /// @author kongweiguang
    pub(crate) fn set_search_suspended(&mut self, suspended: bool, cx: &mut Context<Self>) -> bool {
        self.search.update(cx, |search, cx| {
            if suspended {
                search.suspend_pending(cx)
            } else {
                search.resume_pending(cx)
            }
        })
    }

    /// 搜索失败清空旧坐标及其 revision，避免错误后残留无法对应当前快照的高亮。
    ///
    /// @author kongweiguang
    pub fn set_search_error(
        &mut self,
        session_id: &SessionId,
        request_id: u64,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id)
            || self.active_search_request_id != request_id
        {
            return;
        }
        self.clear_search_result();
        let message = message.into();
        self.search.update(cx, |search, cx| {
            search.set_error(session_id, request_id, message, cx);
        });
    }

    /// 搜索受 Root 请求状态驱动，按钮禁用只反映真实异步操作。
    pub fn set_search_busy(
        &mut self,
        session_id: &SessionId,
        request_id: u64,
        busy: bool,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id)
            || self.active_search_request_id != request_id
        {
            return;
        }
        self.search.update(cx, |search, cx| {
            search.set_busy(session_id, request_id, busy, cx);
        });
    }

    /// 回投真实 selection 的字符数；不存在文本时隐藏复制操作。
    pub fn set_selection_summary(
        &mut self,
        session_id: &SessionId,
        selected_chars: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.selection.update(cx, |selection, cx| {
            selection.set_selected_chars(session_id, selected_chars, cx);
        });
    }

    /// 仅展示 Root 提供的会话投影，选择器不会从 UI 顺序或活动 Tab 推断广播目标。
    pub fn set_broadcast_targets(
        &mut self,
        targets: Vec<BroadcastTargetProjection>,
        cx: &mut Context<Self>,
    ) {
        self.broadcast
            .update(cx, |picker, cx| picker.set_targets(targets, cx));
    }

    /// 广播状态由 Root 在应用层启动或停止成功后回投，按钮点击本身不代表已生效。
    pub fn set_broadcast_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.broadcast
            .update(cx, |picker, cx| picker.set_active(active, cx));
        self.broadcast_visible = active || self.broadcast_visible;
        cx.notify();
    }

    /// Root 在广播用例期间锁定目标操作，避免相同目标被重复启动或停止。
    pub fn set_broadcast_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.broadcast
            .update(cx, |picker, cx| picker.set_pending(pending, cx));
    }

    /// 报告广播失败且保留服务端确认的 active 状态，供用户修正目标后重试。
    pub fn set_broadcast_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.broadcast
            .update(cx, |picker, cx| picker.set_error(message, cx));
        self.broadcast_visible = true;
        cx.notify();
    }

    /// 只有实际装配日志服务时才显示日志控件，避免出现不可用的假按钮。
    pub fn set_log_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if self.log_available == available {
            return;
        }
        self.log_available = available;
        cx.notify();
    }

    /// 回投日志真实状态并解锁待处理操作。
    pub fn set_log_state(
        &mut self,
        session_id: &SessionId,
        state: SessionLogState,
        cx: &mut Context<Self>,
    ) {
        self.logs
            .update(cx, |logs, cx| logs.set_state(session_id, state, cx));
    }

    /// 显示日志用例失败，保留服务端返回的准确状态而不假装启动或停止成功。
    pub fn set_log_error(
        &mut self,
        session_id: &SessionId,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        self.logs
            .update(cx, |logs, cx| logs.set_error(session_id, message, cx));
    }

    /// 日志状态查询、启动或停止期间避免重复派发并可由 Root 解除 pending。
    pub fn set_log_pending(
        &mut self,
        session_id: &SessionId,
        pending: bool,
        cx: &mut Context<Self>,
    ) {
        self.logs.update(cx, |logs, cx| {
            logs.set_pending(session_id, pending, cx);
        });
    }

    /// 命令块只接收实际 OSC/shell integration 投影；空列表在界面中隐藏。
    pub fn set_command_blocks(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        revision: u64,
        blocks: Vec<CommandBlockProjection>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.command_blocks.update(cx, |timeline, cx| {
            timeline.set_blocks(session_id, session_generation, revision, blocks, cx)
        });
    }

    /// 显示快照读取的脱敏业务错误并保留 rail 可见性，便于用户重试刷新。
    pub fn set_command_block_snapshot_error(
        &mut self,
        session_id: &SessionId,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.command_blocks.update(cx, |timeline, cx| {
            timeline.set_snapshot_error(session_id, message, cx)
        });
    }

    /// 回投可信 transcript 查询结果；不同 pane、generation 或已驱逐区块的迟到正文会被忽略。
    pub fn set_command_block_content(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        block_id: kerminal_application::sessions::CommandBlockId,
        content: super::command_blocks::CommandBlockContent,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.command_blocks.update(cx, |timeline, cx| {
            timeline.set_content_state(session_id, session_generation, block_id, content, cx)
        });
    }

    /// 仅在捕获块仍属于同一 binding 时展示复制/Agent 操作回执。
    pub fn set_command_block_notice(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        block_id: kerminal_application::sessions::CommandBlockId,
        notice: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.command_blocks.update(cx, |timeline, cx| {
            timeline.set_notice(session_id, session_generation, block_id, notice, cx)
        });
    }

    /// 将未知提交风险投影到仍属于当前会话代次的卡片，不把新输出当作写入回执。
    pub(crate) fn mark_agent_submission_unknown(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        block_id: kerminal_application::sessions::CommandBlockId,
        agent_session_id: AgentSessionId,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.command_blocks.update(cx, |timeline, cx| {
            timeline.mark_agent_submission_unknown(
                session_id,
                session_generation,
                block_id,
                agent_session_id,
                cx,
            );
        });
    }

    /// Root 只从发起实体的当前 projection 读取复制内容，不重新查询活动 pane 或终端网格。
    pub(crate) fn command_block_export_text(
        &self,
        block_id: kerminal_application::sessions::CommandBlockId,
        cx: &App,
    ) -> Option<String> {
        self.command_blocks.read(cx).export_text(block_id)
    }

    /// 供 Root 的 clipboard/Agent 动作绑定当前输出代次，避免确认期间内容被替换。
    pub(crate) fn command_block_export_revision(
        &self,
        block_id: kerminal_application::sessions::CommandBlockId,
        cx: &App,
    ) -> Option<u64> {
        self.command_blocks.read(cx).export_revision(block_id)
    }

    /// 按当前卡片状态确认输出已展开，避免 watcher 为隐藏块读取 transcript。
    pub(crate) fn command_block_is_expanded(
        &self,
        block_id: kerminal_application::sessions::CommandBlockId,
        cx: &App,
    ) -> bool {
        self.command_blocks.read(cx).is_expanded(block_id)
    }

    /// 只导出仍属于权威快照的 block 身份，供 Root 裁剪已驱逐区块的异步任务。
    pub(crate) fn command_block_ids(
        &self,
        cx: &App,
    ) -> Vec<kerminal_application::sessions::CommandBlockId> {
        self.command_blocks.read(cx).block_ids()
    }

    /// 仅查询用户明确展开的区块，避免每次输出通知都读取整个 session 的 transcript。
    pub(crate) fn expanded_command_block_ids(
        &self,
        cx: &App,
    ) -> Vec<kerminal_application::sessions::CommandBlockId> {
        self.command_blocks.read(cx).expanded_block_ids()
    }

    /// 判断操作来源是否仍在当前快照中，及时忽略复用组件后的迟到按钮事件。
    pub(crate) fn contains_command_block(
        &self,
        block_id: kerminal_application::sessions::CommandBlockId,
        cx: &App,
    ) -> bool {
        self.command_blocks.read(cx).contains_block(block_id)
    }

    /// 异步操作只能基于当前投影里的 OSC generation，不从 session ID 推断重连代次。
    pub(crate) fn command_block_binding(&self, cx: &App) -> Option<(SessionId, u64)> {
        self.command_blocks
            .read(cx)
            .binding()
            .map(|(session_id, generation)| (session_id.clone(), generation))
    }

    /// 设置命令块时间线的策略可见性；折叠只隐藏投影，不清空当前会话数据。
    pub fn set_command_block_rail_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.command_block_rail_visible == visible {
            return;
        }
        self.command_block_rail_visible = visible;
        cx.notify();
    }

    /// 暴露当前绑定，供宿主在异步返回前检查事件目标是否仍然有效。
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// 仅在投影或可见错误变化时通知，失败时保留上次有效规则并显示不回显输入的提示。
    pub fn update_keyword_rules(
        &mut self,
        settings: TerminalKeywordHighlightSettings,
        theme_mode: ThemeMode,
        appearance: WindowAppearance,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let dark_theme = effective_dark_theme(theme_mode, appearance);
        match self.keyword_overlays.update(settings, dark_theme) {
            Ok(projection_changed) => {
                let error_cleared = self.keyword_error.take().is_some();
                let visible_state_changed = projection_changed || error_cleared;
                if visible_state_changed {
                    cx.notify();
                }
                Ok(visible_state_changed)
            }
            Err(error) => {
                let message = crate::locale::text(
                    "终端关键词设置无效，已保留上一套有效规则",
                    "Terminal keyword settings are invalid; the last valid rules remain active",
                )
                .to_string();
                if self.keyword_error.as_ref() != Some(&message) {
                    self.keyword_error = Some(message);
                    cx.notify();
                }
                Err(error)
            }
        }
    }

    /// 搜索与关键词分别投影到当前网格；持续输出只校验有界行见证，不重扫历史。
    ///
    /// @author kongweiguang
    pub(crate) fn render_overlays(&self, snapshot: &RenderSnapshot) -> Arc<[TerminalCellOverlay]> {
        if self.session_id.as_ref() != Some(&snapshot.session_id) || snapshot.size.columns() == 0 {
            return Arc::from([]);
        }

        let mut overlays = self.search_overlays(snapshot).to_vec();
        overlays.extend(
            self.keyword_overlays
                .project(snapshot)
                .iter()
                .map(|matched| TerminalCellOverlay {
                    range: matched.range.clone(),
                    kind: TerminalOverlayKind::Keyword,
                    style: Some(TerminalOverlayStyle {
                        foreground: matched.style.foreground,
                        background: matched.style.background,
                    }),
                }),
        );
        Arc::from(overlays)
    }

    /// 所有结束、切换及新查询路径一起清除结果和缓存，避免保留已失效的行见证。
    fn clear_search_result(&mut self) {
        self.search_result = None;
        self.search_result_revision = None;
        *self.search_overlay_cache.get_mut() = None;
    }

    /// 真实引擎结果按行见证映射新坐标；无见证结果仅允许原 revision，不能猜测有效性。
    fn search_overlays(&self, snapshot: &RenderSnapshot) -> Arc<[TerminalCellOverlay]> {
        let Some(result) = self.search_result.as_ref() else {
            return Arc::from([]);
        };
        if let Some(cache) = self.search_overlay_cache.borrow().as_ref()
            && cache.matches(snapshot)
        {
            return cache.overlays.clone();
        }
        let projected = if result.projection.is_some() {
            result.project_visible_matches(snapshot)
        } else if crate::terminal_canvas::search_result_revision_matches(
            self.search_result_revision,
            snapshot.revision,
        ) {
            Some(Arc::from(
                result
                    .matches
                    .iter()
                    .cloned()
                    .enumerate()
                    .collect::<Vec<_>>(),
            ))
        } else {
            None
        };
        let mut overlays = Vec::new();
        if let Some(projected) = projected {
            for (match_index, matched) in projected.iter() {
                for row in snapshot.rows.iter() {
                    let line = row.absolute_line;
                    if line < matched.range.start.line || line > matched.range.end.line {
                        continue;
                    }
                    let start_column = if line == matched.range.start.line {
                        matched.range.start.column.min(snapshot.size.columns())
                    } else {
                        0
                    };
                    let end_column = if line == matched.range.end.line {
                        matched.range.end.column.min(snapshot.size.columns())
                    } else {
                        snapshot.size.columns()
                    };
                    if start_column >= end_column {
                        continue;
                    }
                    overlays.push(TerminalCellOverlay {
                        range: TerminalPoint {
                            line,
                            column: start_column,
                        }..TerminalPoint {
                            line,
                            column: end_column,
                        },
                        kind: if result.selected_match == Some(*match_index) {
                            TerminalOverlayKind::ActiveSearch
                        } else {
                            TerminalOverlayKind::Search
                        },
                        style: None,
                    });
                }
            }
        }
        let overlays: Arc<[TerminalCellOverlay]> = Arc::from(overlays);
        *self.search_overlay_cache.borrow_mut() = Some(SearchOverlayCache {
            snapshot: snapshot.clone(),
            overlays: overlays.clone(),
        });
        overlays
    }

    /// 画布先把像素换算为 viewport cell，再经统一事件将选择写入当前 session。
    ///
    /// @author kongweiguang
    pub fn emit_viewport_selection(
        &mut self,
        rows: &[TerminalRow],
        columns: u16,
        start: ViewportCell,
        end: ViewportCell,
        kind: TerminalSelectionKind,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(session_id), Some(selection)) = (
            self.session_id.clone(),
            selection_from_viewport_drag(rows, columns, start, end, kind),
        ) else {
            return false;
        };
        cx.emit(TerminalToolsEvent::SetSelection {
            session_id,
            selection: Some(selection),
            gesture_id: None,
            started: false,
            finalized: true,
            expected_revision: None,
            viewport: None,
        });
        true
    }

    /// 保留原有拖动入口；默认不附加清除意图，避免其他调用方意外改变选择语义。
    ///
    /// @author kongweiguang
    pub fn begin_terminal_selection_drag(
        &mut self,
        session_id: SessionId,
        start: TerminalPoint,
        open_link_on_click: bool,
    ) -> bool {
        self.begin_terminal_selection_drag_with_clear(session_id, start, open_link_on_click, false)
    }

    /// 为当前 pane/session 保存按下时核验的清除意图，清除决策留到 release 以避免事件重排。
    ///
    /// @author kongweiguang
    pub fn begin_terminal_selection_drag_with_clear(
        &mut self,
        session_id: SessionId,
        start: TerminalPoint,
        open_link_on_click: bool,
        clear_selection_on_click: bool,
    ) -> bool {
        self.selection_autoscroll_last = None;
        self.selection_autoscroll_scheduled = None;
        self.scrollbar_drag = None;
        if self.session_id.as_ref() != Some(&session_id) {
            self.selection_drag = None;
            self.selection_drag_generation = self.selection_drag_generation.wrapping_add(1);
            return false;
        }
        self.selection_drag_generation = self.selection_drag_generation.wrapping_add(1);
        self.selection_drag = Some(ViewportSelectionDrag::new_with_clear_selection_on_click(
            session_id,
            start,
            open_link_on_click,
            clear_selection_on_click,
        ));
        true
    }

    /// 为 Canvas 建立 owner-bound anchor 并冻结坐标、revision、viewport 与点击策略，避免异步端点交叉。
    /// 保留该边界参数组可避免临时 DTO 穿过 Canvas 调用链，也防止字段从不同按下事件拼接。
    ///
    /// @author kongweiguang
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_canvas_selection_gesture(
        &mut self,
        binding: UiSessionBinding,
        start: TerminalPoint,
        cell: ViewportCell,
        selection_kind: TerminalSelectionKind,
        expected_revision: u64,
        viewport: SelectionViewport,
        open_link_on_click: bool,
        clear_selection_on_click: bool,
        cx: &mut Context<Self>,
    ) -> Option<TerminalSelectionGestureToken> {
        // 新 press 必须先废弃旧 runtime anchor；否则同一 ToolsView 的迟到 Move 仍可能落入旧手势。
        if self.selection_drag.is_some() {
            self.cancel_canvas_selection_gesture(cx);
        }
        self.selection_autoscroll_last = None;
        self.selection_autoscroll_scheduled = None;
        self.scrollbar_drag = None;
        if self.session_id.as_ref() != Some(&binding.session_id) {
            self.cancel_viewport_selection_drag();
            return None;
        }
        self.selection_drag_generation = self.selection_drag_generation.wrapping_add(1);
        let generation = self.selection_drag_generation;
        let token = TerminalSelectionGestureToken::for_canvas_gesture(binding.clone(), generation);
        self.selection_drag = Some(ViewportSelectionDrag::for_canvas_gesture(
            binding.clone(),
            generation,
            token.gesture_id(),
            start,
            cell,
            selection_kind,
            open_link_on_click,
            clear_selection_on_click,
        ));
        cx.emit(TerminalToolsEvent::SetSelection {
            session_id: binding.session_id,
            selection: Some(TerminalSelection {
                start,
                end: start,
                kind: selection_kind,
            }),
            gesture_id: Some(token.gesture_id()),
            started: true,
            finalized: false,
            expected_revision: Some(expected_revision),
            viewport: Some(viewport),
        });
        Some(token)
    }

    /// 仅接受同一 binding/代次的指针更新；变化时将绝对 endpoint 与映射它的当前 viewport 一起发出。
    ///
    /// @author kongweiguang
    pub(crate) fn update_canvas_selection_gesture(
        &mut self,
        token: &TerminalSelectionGestureToken,
        end: TerminalPoint,
        cell: ViewportCell,
        edge_direction: i8,
        viewport: SelectionViewport,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session_id.as_ref() != Some(&token.binding().session_id) {
            return false;
        }
        let Some(drag) = self.selection_drag.as_mut() else {
            return false;
        };
        let Some(_) = drag.frame_state(token.binding(), token.generation()) else {
            return false;
        };
        if let Some(selection) = drag.update(end, cell, edge_direction, &viewport) {
            cx.emit(TerminalToolsEvent::SetSelection {
                session_id: token.binding().session_id.clone(),
                selection: Some(selection),
                gesture_id: Some(token.gesture_id()),
                started: false,
                finalized: false,
                expected_revision: None,
                viewport: Some(viewport),
            });
        }
        if edge_direction == 0 {
            self.selection_autoscroll_scheduled = None;
        }
        true
    }

    /// 滚动请求在新 offset 快照提交前只更新鼠标 cell/边缘态，不伪造选择行号。
    ///
    /// @author kongweiguang
    pub(crate) fn update_canvas_selection_pointer(
        &mut self,
        token: &TerminalSelectionGestureToken,
        cell: ViewportCell,
        edge_direction: i8,
    ) -> bool {
        if self.session_id.as_ref() != Some(&token.binding().session_id) {
            return false;
        }
        let Some(drag) = self.selection_drag.as_mut() else {
            return false;
        };
        if drag
            .frame_state(token.binding(), token.generation())
            .is_none()
        {
            return false;
        }
        drag.update_pointer(cell, edge_direction);
        true
    }

    /// 为指定手势登记唯一在途自动滚动；request id 将它与异步 Root ACK 精确关联。
    ///
    /// @author kongweiguang
    pub(crate) fn register_canvas_selection_scroll(
        &mut self,
        binding: &UiSessionBinding,
        gesture_id: u64,
        offset: usize,
        requested_at: Instant,
    ) -> Option<u64> {
        if self.session_id.as_ref() != Some(&binding.session_id) {
            return None;
        }
        let drag = self.selection_drag.as_mut()?;
        drag.register_pending_scroll(binding, gesture_id, offset, requested_at)
    }

    /// 只保存精确 owner、gesture 和 request 的成功 ACK；后续要等 Canvas 实际持有该 revision 快照。
    ///
    /// @author kongweiguang
    pub(crate) fn acknowledge_canvas_selection_scroll(
        &mut self,
        binding: &UiSessionBinding,
        gesture_id: u64,
        request_id: u64,
        min_snapshot_revision: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session_id.as_ref() != Some(&binding.session_id) {
            return false;
        }
        let acknowledged = self.selection_drag.as_mut().is_some_and(|drag| {
            drag.acknowledge_pending_scroll(binding, gesture_id, request_id, min_snapshot_revision)
        });
        if acknowledged {
            cx.notify();
        }
        acknowledged
    }

    /// 真实滚动 ACK 与最小 snapshot revision 到达后恢复从当前快照更新端点。
    ///
    /// @author kongweiguang
    pub(crate) fn clear_canvas_pending_scroll(
        &mut self,
        token: &TerminalSelectionGestureToken,
    ) -> bool {
        let Some(drag) = self.selection_drag.as_mut() else {
            return false;
        };
        if drag
            .frame_state(token.binding(), token.generation())
            .is_none()
        {
            return false;
        }
        drag.clear_pending_scroll();
        true
    }

    /// release 撞上在途滚动时保留 release 信息，待新 offset 快照后再提交最终绝对行。
    ///
    /// @author kongweiguang
    pub(crate) fn defer_canvas_selection_release(
        &mut self,
        token: &TerminalSelectionGestureToken,
        cell: ViewportCell,
        plain_unmodified_release: bool,
        modifier_is_held: bool,
        click_count: usize,
    ) -> bool {
        let Some(drag) = self.selection_drag.as_mut() else {
            return false;
        };
        if drag
            .frame_state(token.binding(), token.generation())
            .is_none()
        {
            return false;
        }
        drag.defer_release_until_scroll_ack(
            cell,
            plain_unmodified_release,
            modifier_is_held,
            click_count,
        );
        true
    }

    /// 自动滚动逐帧从 view 读取小型坐标状态，避免长寿命回调持有过期 snapshot 或 Canvas geometry。
    ///
    /// @author kongweiguang
    pub(crate) fn canvas_selection_gesture_frame(
        &self,
        token: &TerminalSelectionGestureToken,
    ) -> Option<TerminalSelectionGestureFrame> {
        if self.session_id.as_ref() != Some(&token.binding().session_id) {
            return None;
        }
        self.selection_drag
            .as_ref()?
            .frame_state(token.binding(), token.generation())
    }

    /// 只把当前 binding 对应的拖动 token 暴露给 Canvas，鼠标 move 不从活动 pane 猜目标。
    ///
    /// @author kongweiguang
    pub(crate) fn active_canvas_selection_gesture(
        &self,
        binding: &UiSessionBinding,
    ) -> Option<TerminalSelectionGestureToken> {
        if self.session_id.as_ref() != Some(&binding.session_id) {
            return None;
        }
        self.selection_drag.as_ref()?.token_for_binding(binding)
    }

    /// 给 Root 的 Ctrl+C gate 使用；旧或其他 pane 的手势不阻止当前 binding 的中断键。
    ///
    /// @author kongweiguang
    pub(crate) fn has_active_selection_gesture(&self, binding: &UiSessionBinding) -> bool {
        self.active_canvas_selection_gesture(binding).is_some()
    }

    /// 不同 Canvas 命中时撤销旧 owner 的拖拽，避免跨 pane 的指针移动继续驱动旧视口。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_canvas_interaction_for_other_binding(
        &mut self,
        binding: Option<&UiSessionBinding>,
        cx: &mut Context<Self>,
    ) {
        let selection_mismatch = self
            .selection_drag
            .as_ref()
            .is_some_and(|drag| !drag.belongs_to_binding(binding));
        let scrollbar_mismatch = self
            .scrollbar_drag
            .as_ref()
            .is_some_and(|drag| Some(&drag.binding) != binding);
        if selection_mismatch || scrollbar_mismatch {
            self.cancel_canvas_selection_gesture(cx);
        }
    }

    /// 取消未完成的 Canvas 手势并通知 Root 废弃同一 anchor token；已提交的选择保持不变。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_canvas_selection_gesture(&mut self, cx: &mut Context<Self>) {
        if let Some((session_id, gesture_id)) = self
            .selection_drag
            .as_ref()
            .and_then(ViewportSelectionDrag::canvas_cancellation_identity)
        {
            cx.emit(TerminalToolsEvent::CancelSelectionGesture {
                session_id,
                gesture_id,
            });
        }
        self.cancel_viewport_selection_drag();
    }

    /// Root 的迟到 ACK 只能取消其原 gesture，不能清掉之后刚启动的新鼠标拖选。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_canvas_selection_gesture_if(
        &mut self,
        gesture_id: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let matches = self
            .selection_drag
            .as_ref()
            .and_then(ViewportSelectionDrag::canvas_cancellation_identity)
            .is_some_and(|(_, current)| current == gesture_id);
        if matches {
            self.cancel_canvas_selection_gesture(cx);
        }
        matches
    }

    /// 同一手势仅排入一条动画链；滚动 ACK 在途时即使指针回到画布内也需保留该帧链。
    ///
    /// @author kongweiguang
    pub(crate) fn begin_canvas_autoscroll_schedule(
        &mut self,
        token: &TerminalSelectionGestureToken,
    ) -> bool {
        if self
            .canvas_selection_gesture_frame(token)
            .is_none_or(|frame| frame.edge_direction == 0 && frame.pending_scroll.is_none())
        {
            return false;
        }
        let key = (token.binding().clone(), token.generation());
        if self.selection_autoscroll_scheduled.as_ref() == Some(&key) {
            return false;
        }
        self.selection_autoscroll_scheduled = Some(key);
        true
    }

    /// 只清除本次 owner/代次的动画链标记，迟到旧帧不能撤销新拖动排队的链。
    ///
    /// @author kongweiguang
    pub(crate) fn finish_canvas_autoscroll_schedule(
        &mut self,
        token: &TerminalSelectionGestureToken,
    ) {
        let key = (token.binding().clone(), token.generation());
        if self.selection_autoscroll_scheduled.as_ref() == Some(&key) {
            self.selection_autoscroll_scheduled = None;
        }
    }

    /// 只在同一拖选仍停留于边缘时按 50ms 节拍允许一次滚动，避免高刷新率机器把历史瞬间冲过。
    ///
    /// @author kongweiguang
    pub(crate) fn should_autoscroll_canvas_gesture(
        &mut self,
        token: &TerminalSelectionGestureToken,
        now: Instant,
    ) -> bool {
        if self
            .canvas_selection_gesture_frame(token)
            .is_none_or(|frame| frame.edge_direction == 0)
        {
            self.selection_autoscroll_last = None;
            return false;
        }
        let elapsed = self
            .selection_autoscroll_last
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_millis(50));
        if elapsed {
            self.selection_autoscroll_last = Some(now);
        }
        elapsed
    }

    /// 完成前核 owner 和代次，并把 End endpoint 所属 viewport 一起发出，避免 resize/scroll 下重用旧映射。
    ///
    /// @author kongweiguang
    pub(crate) fn finish_canvas_selection_gesture(
        &mut self,
        token: &TerminalSelectionGestureToken,
        end: TerminalPoint,
        viewport: SelectionViewport,
        plain_unmodified_release: bool,
        cx: &mut Context<Self>,
    ) -> Option<CompletedViewportSelectionDrag> {
        if self.session_id.as_ref() != Some(&token.binding().session_id)
            || self
                .selection_drag
                .as_ref()?
                .frame_state(token.binding(), token.generation())
                .is_none()
        {
            return None;
        }
        self.selection_drag_generation = self.selection_drag_generation.wrapping_add(1);
        self.selection_autoscroll_last = None;
        self.selection_autoscroll_scheduled = None;
        let drag = self.selection_drag.take()?;
        let completed =
            drag.finish_after_release(&token.binding().session_id, end, plain_unmodified_release)?;
        cx.emit(TerminalToolsEvent::SetSelection {
            session_id: token.binding().session_id.clone(),
            selection: completed.selection,
            gesture_id: Some(token.gesture_id()),
            started: false,
            finalized: true,
            expected_revision: None,
            viewport: Some(viewport),
        });
        if completed.clear_selection {
            cx.emit(TerminalToolsEvent::ClearSelection {
                session_id: token.binding().session_id.clone(),
            });
        }
        Some(completed)
    }

    /// 保留原有结束入口；未显式核验 release 时一律不触发点击清除。
    ///
    /// @author kongweiguang
    pub fn finish_terminal_selection_drag(
        &mut self,
        session_id: &SessionId,
        end: TerminalPoint,
        cx: &mut Context<Self>,
    ) -> Option<CompletedViewportSelectionDrag> {
        self.finish_terminal_selection_drag_after_release(session_id, end, false, cx)
    }

    /// 完成同 session 手势后只发一种选择事件；清理需按下和抬起都核验为普通空白单击。
    ///
    /// @author kongweiguang
    pub fn finish_terminal_selection_drag_after_release(
        &mut self,
        session_id: &SessionId,
        end: TerminalPoint,
        plain_unmodified_release: bool,
        cx: &mut Context<Self>,
    ) -> Option<CompletedViewportSelectionDrag> {
        if self.session_id.as_ref() != Some(session_id) {
            self.selection_drag = None;
            return None;
        }
        let drag = self.selection_drag.take()?;
        let completed = drag.finish_after_release(session_id, end, plain_unmodified_release)?;
        if let Some(selection) = completed.selection {
            cx.emit(TerminalToolsEvent::SetSelection {
                session_id: session_id.clone(),
                selection: Some(selection),
                gesture_id: None,
                started: false,
                finalized: true,
                expected_revision: None,
                viewport: None,
            });
        } else if completed.clear_selection {
            cx.emit(TerminalToolsEvent::ClearSelection {
                session_id: session_id.clone(),
            });
        }
        Some(completed)
    }

    /// 终端进入鼠标协议模式或失去目标时清除未完成拖动，避免后续误提交旧坐标。
    ///
    /// @author kongweiguang
    pub fn cancel_viewport_selection_drag(&mut self) {
        self.selection_drag = None;
        self.selection_drag_generation = self.selection_drag_generation.wrapping_add(1);
        self.selection_autoscroll_last = None;
        self.selection_autoscroll_scheduled = None;
        self.scrollbar_drag = None;
    }

    /// 固定滚动条按下时的 offset 与 thumb 行程，避免拖动期间布局重绘改变比例锚点。
    ///
    /// @author kongweiguang
    pub(crate) fn begin_terminal_scrollbar_drag(
        &mut self,
        binding: UiSessionBinding,
        pointer_y: f32,
        offset: usize,
        max_offset: usize,
        thumb_travel: f32,
    ) -> bool {
        if self.session_id.as_ref() != Some(&binding.session_id)
            || !pointer_y.is_finite()
            || !thumb_travel.is_finite()
            || thumb_travel <= 0.0
            || max_offset == 0
        {
            self.scrollbar_drag = None;
            return false;
        }
        self.selection_drag = None;
        self.selection_drag_generation = self.selection_drag_generation.wrapping_add(1);
        self.selection_autoscroll_last = None;
        self.selection_autoscroll_scheduled = None;
        self.scrollbar_drag = Some(TerminalScrollbarDrag {
            binding,
            initial_offset: offset.min(max_offset),
            max_offset,
            pointer_start_y: pointer_y,
            thumb_travel,
        });
        true
    }

    /// 拖动期间按 owner 核验并把像素位移换算为 offset；恶意 NaN 或零行程不改变视口。
    ///
    /// @author kongweiguang
    pub(crate) fn update_terminal_scrollbar_drag(
        &self,
        binding: &UiSessionBinding,
        pointer_y: f32,
    ) -> Option<usize> {
        let drag = self.scrollbar_drag.as_ref()?;
        if &drag.binding != binding || !pointer_y.is_finite() || drag.thumb_travel <= 0.0 {
            return None;
        }
        scrollbar_drag_offset(
            drag.initial_offset,
            pointer_y - drag.pointer_start_y,
            drag.max_offset,
            drag.thumb_travel,
        )
    }

    /// 抬起或离开时清除滚动条锚点，避免下一次终端点击继承旧拖动。
    ///
    /// @author kongweiguang
    pub(crate) fn finish_terminal_scrollbar_drag(&mut self, binding: &UiSessionBinding) -> bool {
        if self
            .scrollbar_drag
            .as_ref()
            .is_some_and(|drag| &drag.binding == binding)
        {
            self.scrollbar_drag = None;
            true
        } else {
            false
        }
    }

    /// 终端 OSC 8 点击必须先通过 HTTP(S) 白名单，再把不可伪造的安全链接交给 Root。
    pub fn emit_open_link(
        &mut self,
        raw_uri: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), super::TerminalLinkError> {
        let link = validate_terminal_link(raw_uri)?;
        cx.emit(TerminalToolsEvent::OpenLink { link });
        Ok(())
    }

    /// 关闭结束查询组合态、清除高亮并作废旧结果；Root 只向仍有效的同一 pane 恢复焦点。
    ///
    /// @author kongweiguang
    pub(crate) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_search_request_id = self.active_search_request_id.wrapping_add(1);
        self.search_visible = false;
        self.clear_search_result();
        self.search.update(cx, |search, cx| {
            search.dismiss_local(window, cx);
        });
        if let Some(session_id) = self.session_id.clone() {
            cx.emit(TerminalToolsEvent::CloseSearch { session_id });
        }
        cx.notify();
    }

    /// 活跃广播不能因面板折叠被隐式停止；取消草稿时才清除显式目标选择。
    fn toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        let is_active = self.broadcast_active(cx);
        if is_active || self.broadcast.read(cx).is_pending() {
            self.broadcast_visible = true;
        } else if self.broadcast_visible {
            self.broadcast_visible = false;
            self.broadcast
                .update(cx, |picker, cx| picker.cancel_selection(cx));
        } else {
            self.broadcast_visible = true;
        }
        cx.notify();
    }

    /// 从 picker 读取真实活动状态，避免仅靠右栏按钮推测 Root 的广播状态。
    fn broadcast_active(&self, cx: &Context<Self>) -> bool {
        self.broadcast.read(cx).is_active()
    }
}

impl Render for TerminalToolsView {
    /// 搜索栏只由 pane 顶部 overlay 挂载；右栏保留搜索开关及其它真实工具投影。
    /// 所有副作用仍经 Root 和 Application 服务，避免同一搜索 Entity 被重复渲染。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let selection = self.selection.clone();
        let broadcast = self.broadcast.clone();
        let logs = self.logs.clone();
        let command_blocks = self.command_blocks.clone();
        let keyword_error = self.keyword_error.clone();
        let show_search = self.search_visible;
        let show_broadcast = self.broadcast_visible;
        let show_logs = self.log_available && self.session_id.is_some();
        let has_command_blocks = super::command_blocks::command_block_timeline_visible(
            self.command_block_rail_visible,
            !self.command_blocks.read(cx).is_empty(),
        );
        let search_label = if show_search {
            crate::locale::text("收起搜索", "Hide search")
        } else {
            crate::locale::text("搜索终端", "Search terminal")
        };
        let broadcast_label = if self.broadcast_active(cx) {
            crate::locale::text("广播已启用", "Broadcast is active")
        } else if show_broadcast {
            crate::locale::text("取消广播选择", "Cancel broadcast selection")
        } else {
            crate::locale::text("选择广播目标", "Choose broadcast targets")
        };

        let panel = div()
            .id("terminal-tools-view")
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(theme.materials.panel)
            .text_color(theme.foreground)
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .bg(theme.materials.chrome)
                    .child(
                        Button::new("terminal-tools-search-toggle")
                            .compact()
                            .label(search_label)
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                if view.search_visible {
                                    view.close_search(window, cx);
                                } else {
                                    let session_id = view.session_id.clone();
                                    if let Some(session_id) = session_id {
                                        view.open_search_for(&session_id, window, cx);
                                    }
                                }
                            })),
                    )
                    .child(
                        Button::new("terminal-tools-broadcast-toggle")
                            .compact()
                            .label(broadcast_label)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.toggle_broadcast(cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(selection),
            );

        let mut content = div()
            .id("terminal-tools-content")
            .w_full()
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        if let Some(error) = keyword_error {
            content = content.child(
                div()
                    .id("terminal-tools-keyword-error")
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .text_color(theme.error)
                    .child(error),
            );
        }
        if show_broadcast {
            content = content.child(broadcast);
        }
        if show_logs {
            content = content.child(logs);
        }
        if has_command_blocks {
            content = content.child(command_blocks);
        }
        panel.child(content)
    }
}

/// 解析用户主题策略；System 仅在系统深色外观下选择深色关键词调色板。
fn effective_dark_theme(mode: ThemeMode, appearance: WindowAppearance) -> bool {
    match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    }
}
