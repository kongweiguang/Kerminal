//! Agent 终端的视图订阅与生命周期；会话资源仍由共享 SessionService 持有。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{AppContext as _, Context, FocusHandle, Window};
use kerminal_application::{
    AppError, AppResult, PublicErrorCode, SessionService,
    sessions::{SessionChangeKind, SessionFilter, TerminalCommandContext, UiSessionBinding},
};
use kerminal_domain::{
    identity::{AgentSessionId, SessionId},
    sessions::{SessionInfo, SessionLifecycle, SessionOwner},
};
use kerminal_terminal::snapshot::{RenderSnapshot, RenderSnapshotRequest, TerminalPalette};
use tokio_util::sync::CancellationToken;

use super::{
    RootView, command_blocks, preferences, terminal_open::lifecycle_retains_terminal_history,
};

/// 一次真实读取的投影；保留已知生命周期，避免快照失败反而使已退出进程恢复输入。
struct AgentTerminalFrame {
    lifecycle: Option<SessionLifecycle>,
    snapshot: Option<RenderSnapshot>,
    input_context: Option<TerminalCommandContext>,
    failure: Option<AppError>,
}

#[derive(Clone, Copy)]
struct AgentTerminalPaletteTarget {
    palette: TerminalPalette,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentFrameAcceptance {
    Accept,
    RetryWithCurrentPalette,
    StopStaleWatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentWatchProgress {
    Continue,
    RetryWithCurrentPalette,
    Stop,
}

/// 创建回执只保留一个短期焦点请求；用户焦点意图由启动时的 pane generation 与 FocusHandle 锚定。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct PendingAgentFocusIntent {
    request_generation: u64,
    pane_focus_generation: u64,
    expected_focus: Option<FocusHandle>,
    selected_agent_at_start: Option<AgentSessionId>,
    selected_terminal_at_start: Option<SessionId>,
    target: Option<(AgentSessionId, SessionId)>,
}

/// 创建异步期间重复投影原选择不代表用户改焦点；一旦选择离开基线或已就绪的新目标就撤销。
///
/// @author kongweiguang
fn agent_creation_selection_matches(
    intent: &PendingAgentFocusIntent,
    agent_session_id: Option<&AgentSessionId>,
    session_id: Option<&SessionId>,
) -> bool {
    if let Some((target_agent, target_session)) = intent.target.as_ref() {
        return Some(target_agent) == agent_session_id && Some(target_session) == session_id;
    }
    intent.selected_agent_at_start.as_ref() == agent_session_id
        && intent.selected_terminal_at_start.as_ref() == session_id
}

/// 请求必须命中同一 Agent、PTY 和固定 handler binding，防止迟到 watcher 借当前选择重定向。
///
/// @author kongweiguang
fn agent_creation_focus_binding_matches(
    target: &(AgentSessionId, SessionId),
    agent_session_id: &AgentSessionId,
    session_id: &SessionId,
    binding: &UiSessionBinding,
) -> bool {
    target.0 == *agent_session_id
        && target.1 == *session_id
        && binding.session_id == *session_id
        && matches!(
            &binding.owner,
            SessionOwner::Agent { agent_session_id: owner_id } if owner_id == agent_session_id
        )
}

/// Running 和 Exited 保留可读快照订阅；其它生命周期不再查询引擎或等待终端变化。
///
/// @author kongweiguang
fn agent_lifecycle_allows_snapshot(lifecycle: SessionLifecycle) -> bool {
    matches!(
        lifecycle,
        SessionLifecycle::Running | SessionLifecycle::Exited
    )
}

/// Agent 退出后只保留最后输出与主题刷新，不能重新建立 PTY 输入上下文。
///
/// @author kongweiguang
fn agent_lifecycle_allows_input(lifecycle: SessionLifecycle) -> bool {
    lifecycle == SessionLifecycle::Running
}

/// 只有主题代次、快照实际 palette 与 watcher 所有权都匹配时才允许提交，隔离迟到的底层重配。
///
/// @author kongweiguang
fn agent_frame_acceptance(
    watch_is_current: bool,
    captured_palette_generation: u64,
    current_palette_generation: u64,
    snapshot_palette_matches: Option<bool>,
) -> AgentFrameAcceptance {
    if !watch_is_current {
        AgentFrameAcceptance::StopStaleWatch
    } else if captured_palette_generation != current_palette_generation
        || snapshot_palette_matches == Some(false)
    {
        AgentFrameAcceptance::RetryWithCurrentPalette
    } else {
        AgentFrameAcceptance::Accept
    }
}

/// 按 theme revision 去重配色写入；新 watcher 和主题变更仍各自同步一次。
///
/// @author kongweiguang
fn agent_palette_update_required(applied_generation: Option<u64>, target_generation: u64) -> bool {
    applied_generation != Some(target_generation)
}

/// 只把 registry 中同 Agent、同 PTY 的真实 Exited 当作读取输入上下文期间的关闭竞态。
/// Running 冲突、其它 owner 或旧 PTY 不得靠活动目标猜测消解，真实快照错误也不在此清除。
///
/// @author kongweiguang
fn agent_context_conflict_is_confirmed_exit(
    sessions: &[SessionInfo],
    owner: &SessionOwner,
    session_id: &SessionId,
) -> bool {
    sessions.iter().any(|session| {
        session.session_id == *session_id
            && session.owner == *owner
            && session.lifecycle == SessionLifecycle::Exited
    })
}

/// 首帧、主题代次变化和 Lifecycle 通知读取 registry；普通输出只取画布，避免逐字查询会话。
/// 配色仅在 watcher 首次绑定或主题代次变化时写入；NotFound 收敛为 Closed，其它错误保留给 Agent 面板。
/// list 与输入上下文读取之间可能正常退出，Conflict 须用同 owner/PTY 的真实退出再核对，不能误报运行故障。
///
/// @author kongweiguang
async fn read_agent_terminal_frame(
    sessions: Arc<dyn SessionService>,
    agent_session_id: AgentSessionId,
    session_id: SessionId,
    request: RenderSnapshotRequest,
    refresh_lifecycle: bool,
    palette: Option<TerminalPalette>,
) -> AppResult<AgentTerminalFrame> {
    let owner = SessionOwner::Agent { agent_session_id };
    let lifecycle = if refresh_lifecycle {
        Some(
            sessions
                .list(SessionFilter {
                    owner: Some(owner.clone()),
                    ..SessionFilter::default()
                })
                .await?
                .into_iter()
                .find(|session| session.session_id == session_id)
                .map_or(SessionLifecycle::Closed, |session| session.lifecycle),
        )
    } else {
        None
    };
    let mut frame = AgentTerminalFrame {
        lifecycle,
        snapshot: None,
        input_context: None,
        failure: None,
    };
    if lifecycle.is_some_and(|lifecycle| !agent_lifecycle_allows_snapshot(lifecycle)) {
        return Ok(frame);
    }
    if let Some(palette) = palette {
        match sessions.set_palette(session_id.clone(), palette).await {
            Ok(()) => {}
            Err(error) if error.public_code() == PublicErrorCode::NotFound => {
                frame.lifecycle = Some(SessionLifecycle::Closed);
                return Ok(frame);
            }
            Err(error) => return Err(error),
        }
    }
    match sessions.render_snapshot(session_id.clone(), request).await {
        Ok(snapshot) => frame.snapshot = Some(snapshot),
        Err(error) if error.public_code() == PublicErrorCode::NotFound => {
            frame.lifecycle = Some(SessionLifecycle::Closed);
            return Ok(frame);
        }
        Err(error) => frame.failure = Some(error),
    }
    if lifecycle.is_some_and(agent_lifecycle_allows_input) {
        match sessions.terminal_command_context(session_id.clone()).await {
            Ok(context) if context.session_id == session_id && context.owner == owner => {
                frame.input_context = Some(context);
            }
            Ok(_) => {
                frame.failure = Some(AppError::conflict("Agent 终端归属已变化，输入已停用"));
            }
            Err(error) if error.public_code() == PublicErrorCode::NotFound => {
                frame.lifecycle = Some(SessionLifecycle::Closed);
            }
            Err(error) if error.public_code() == PublicErrorCode::Conflict => {
                let current = sessions
                    .list(SessionFilter {
                        owner: Some(owner.clone()),
                        ..SessionFilter::default()
                    })
                    .await;
                if current.as_ref().is_ok_and(|current| {
                    agent_context_conflict_is_confirmed_exit(current, &owner, &session_id)
                }) {
                    frame.lifecycle = Some(SessionLifecycle::Exited);
                } else {
                    frame.failure.get_or_insert(error);
                }
            }
            Err(error) => frame.failure = Some(error),
        }
    }
    Ok(frame)
}

impl RootView {
    /// 代次、Agent 与 PTY 身份必须同时匹配；迟到结果不能按当前焦点寻找替代目标。
    ///
    /// @author kongweiguang
    fn agent_terminal_watch_is_current(
        &self,
        generation: u64,
        agent_session_id: &AgentSessionId,
        session_id: &SessionId,
    ) -> bool {
        self.agent_watch_generation == generation
            && self.selected_agent_session_id.as_ref() == Some(agent_session_id)
            && self.agent_terminal_session_id.as_ref() == Some(session_id)
    }

    /// 创建开始时只接受可见 Agent 工具面板与非模态界面，并记录原焦点和 pane 意图代次。
    ///
    /// @author kongweiguang
    pub(super) fn begin_agent_creation_focus_intent(
        &mut self,
        request_generation: u64,
        window: &Window,
        cx: &Context<Self>,
    ) {
        self.pending_agent_focus_intent = None;
        if !self.agent_focus_surface_available(window, cx, true) {
            return;
        }
        self.pending_agent_focus_intent = Some(PendingAgentFocusIntent {
            request_generation,
            pane_focus_generation: self.pane_focus_intent_generation,
            expected_focus: window.focused(cx),
            selected_agent_at_start: self.selected_agent_session_id.clone(),
            selected_terminal_at_start: self.agent_terminal_session_id.clone(),
            target: None,
        });
    }

    /// 新建回执只补齐同代次的精确 PTY；共享抽屉焦点不代表用户未导航，必须同时核对既有导航代次。
    /// 如果 handler 已就绪则立即走一次消费路径，不重新绑定目标。
    ///
    /// @author kongweiguang
    pub(super) fn complete_agent_creation_focus_intent(
        &mut self,
        request_generation: u64,
        agent_session_id: AgentSessionId,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let intent_is_current = self
            .pending_agent_focus_intent
            .as_ref()
            .is_some_and(|intent| intent.request_generation == request_generation);
        if !intent_is_current {
            return;
        }
        let original_focus = self
            .pending_agent_focus_intent
            .as_ref()
            .is_some_and(|intent| {
                agent_creation_focus_source_is_current(
                    intent.pane_focus_generation,
                    self.pane_focus_intent_generation,
                    intent.expected_focus == window.focused(cx),
                )
            });
        if !original_focus || !self.agent_focus_surface_available(window, cx, true) {
            self.pending_agent_focus_intent = None;
            return;
        }
        let target = (agent_session_id.clone(), session_id.clone());
        let conflicting_target = self
            .pending_agent_focus_intent
            .as_ref()
            .is_some_and(|intent| {
                intent
                    .target
                    .as_ref()
                    .is_some_and(|existing| existing != &target)
            });
        if conflicting_target {
            self.pending_agent_focus_intent = None;
            return;
        }
        let Some(intent) = self.pending_agent_focus_intent.as_mut() else {
            return;
        };
        intent.target = Some(target);
        let watch_generation = self.agent_watch_generation;
        self.try_consume_agent_creation_focus_intent(
            watch_generation,
            &agent_session_id,
            &session_id,
            window,
            cx,
        );
    }

    /// 失败或显式取消只撤销对应创建请求，迟到的旧取消不会清除后续焦点意图。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_agent_creation_focus_intent(&mut self, request_generation: u64) {
        if self
            .pending_agent_focus_intent
            .as_ref()
            .is_some_and(|intent| intent.request_generation == request_generation)
        {
            self.pending_agent_focus_intent = None;
        }
    }

    /// 普通 Agent 选择会撤销未完成焦点请求；只保留 Ready 已精确指向的新建 Agent 与 PTY。
    ///
    /// @author kongweiguang
    pub(super) fn retain_agent_creation_focus_for_selection(
        &mut self,
        agent_session_id: Option<&AgentSessionId>,
        session_id: Option<&SessionId>,
    ) {
        let selection_matches_intent =
            self.pending_agent_focus_intent
                .as_ref()
                .is_some_and(|intent| {
                    agent_creation_selection_matches(intent, agent_session_id, session_id)
                });
        if !selection_matches_intent {
            self.pending_agent_focus_intent = None;
        }
    }

    /// handler 安装或 Ready 回执后仅消费确切 owner/PTY 的 Running watcher；共享焦点下的后续导航也会作废请求。
    /// 失焦、隐藏和模态仍按既有门禁拒绝，不为迟到回执恢复用户已经离开的面板。
    ///
    /// @author kongweiguang
    pub(super) fn try_consume_agent_creation_focus_intent(
        &mut self,
        watch_generation: u64,
        agent_session_id: &AgentSessionId,
        session_id: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(intent) = self.pending_agent_focus_intent.as_ref().cloned() else {
            return;
        };
        if !self.agent_focus_surface_available(window, cx, true)
            || !agent_creation_focus_source_is_current(
                intent.pane_focus_generation,
                self.pane_focus_intent_generation,
                intent.expected_focus == window.focused(cx),
            )
        {
            self.pending_agent_focus_intent = None;
            return;
        }
        let Some(target) = intent.target.as_ref() else {
            return;
        };
        if target.0 != *agent_session_id || target.1 != *session_id {
            return;
        }
        if !self.agent_terminal_watch_is_current(watch_generation, agent_session_id, session_id) {
            return;
        }
        if self.agent_terminal_lifecycle != Some(SessionLifecycle::Running) {
            if self.agent_terminal_lifecycle.is_some() {
                self.pending_agent_focus_intent = None;
            }
            return;
        }
        let Some(registration) = self.pane_input_handlers.get(session_id) else {
            return;
        };
        if !agent_creation_focus_binding_matches(
            target,
            agent_session_id,
            session_id,
            &registration.binding,
        ) {
            self.pending_agent_focus_intent = None;
            return;
        }
        let handler = registration.entity.clone();
        self.pending_agent_focus_intent = None;
        self.focus_agent_terminal(handler, window, cx);
    }

    /// 自动创建焦点要求窗口仍激活；退出归还只改本窗口内部 focus，不激活 OS 窗口，故可在后台精确归还原 Agent 焦点。
    /// 工具面板必须真实可见，modal、prompt、关闭意图和停机状态均阻断；投影查询可能刷新布局缓存，保留可变借用。
    ///
    /// @author kongweiguang
    fn agent_focus_surface_available(
        &mut self,
        window: &Window,
        cx: &Context<Self>,
        require_active_window: bool,
    ) -> bool {
        (!require_active_window || window.is_window_active())
            && self.agent_tool_panel_is_visible()
            && !self.host_shutdown_requested
            && !super::logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open || self.connection_prompt_modal_open,
                self.snippet_editor_modal_open,
                self.root_confirmation_owner.is_some()
                    || self.split_target_picker.read(cx).is_open()
                    || window.has_active_prompt(),
                self.pending_quit_intent.is_some() || self.quit_ready_intent.is_some(),
            )
    }

    /// PTY watcher 已报告错误或终态时撤销对应新建焦点请求，不留下可被后续无关 handler 消费的意图。
    ///
    /// @author kongweiguang
    fn clear_agent_creation_focus_for_terminal(&mut self, session_id: &SessionId) {
        if self
            .pending_agent_focus_intent
            .as_ref()
            .is_some_and(|intent| {
                intent
                    .target
                    .as_ref()
                    .is_some_and(|(_, target_session_id)| target_session_id == session_id)
            })
        {
            self.pending_agent_focus_intent = None;
        }
    }

    /// watcher 明确失败时只撤销同 Agent 的创建焦点请求，不改变其他终端或 Agent 的反馈状态。
    ///
    /// @author kongweiguang
    fn clear_agent_creation_focus_for_agent(&mut self, agent_session_id: &AgentSessionId) {
        if self
            .pending_agent_focus_intent
            .as_ref()
            .is_some_and(|intent| {
                intent
                    .target
                    .as_ref()
                    .is_some_and(|(target_agent_id, _)| target_agent_id == agent_session_id)
            })
        {
            self.pending_agent_focus_intent = None;
        }
    }

    /// 运行态错误归属于明确 Agent；中央工作区不承担另一会话的失败提示。
    ///
    /// @author kongweiguang
    pub(super) fn report_agent_terminal_failure(
        &mut self,
        agent_session_id: &AgentSessionId,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent_session_id.as_ref() != Some(agent_session_id) {
            return;
        }
        self.clear_agent_creation_focus_for_agent(agent_session_id);
        if let Some(panel) = self.agents_panel.as_ref() {
            panel.update(cx, |panel, cx| {
                panel.report_terminal_failure(agent_session_id, message, cx);
            });
        }
    }

    /// 输入/控制项的拒绝与 watcher 读取故障分开投影；只有真实退出确认才能消解关闭竞态的拒绝。
    /// 保留错误码而非匹配本地化字符串，Unknown 或基础设施失败不能被成功退出掩盖。
    ///
    /// @author kongweiguang
    pub(super) fn report_agent_terminal_operation_failure(
        &mut self,
        agent_session_id: &AgentSessionId,
        error: &AppError,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent_session_id.as_ref() != Some(agent_session_id) {
            return;
        }
        if let Some(panel) = self.agents_panel.as_ref() {
            panel.update(cx, |panel, cx| {
                panel.report_terminal_operation_failure(
                    agent_session_id,
                    error.public_code(),
                    error.public_message().into(),
                    cx,
                );
            });
        }
    }

    /// 结束时保留最后可读快照和只读 view binding，同时撤销输入与待发命令；不对已退出 PTY 补发 blur 或 resize。
    /// 只有精确 Agent/PTY binding 仍占焦点时才归还到紧凑宿主或 Workspace，不重绑会话。
    /// 历史只读退役复用当前窗口核对物理焦点，避免丢弃 handler 后把焦点留在失效输入实体上。
    ///
    /// @author kongweiguang
    fn complete_agent_terminal_watch(
        &mut self,
        lifecycle: SessionLifecycle,
        agent_session_id: &AgentSessionId,
        session_id: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let exact_binding_is_current = self.selected_agent_session_id.as_ref()
            == Some(agent_session_id)
            && self.agent_terminal_session_id.as_ref() == Some(session_id)
            && self
                .pane_input_handlers
                .get(session_id)
                .is_some_and(|registration| {
                    agent_creation_focus_binding_matches(
                        &(agent_session_id.clone(), session_id.clone()),
                        agent_session_id,
                        session_id,
                        &registration.binding,
                    )
                });
        let terminal_focus_is_current = exact_binding_is_current
            && self
                .pane_input_handlers
                .get(session_id)
                .is_some_and(|registration| {
                    registration
                        .entity
                        .read(cx)
                        .focus_handle()
                        .is_focused(window)
                });
        let focus_surface_available =
            terminal_focus_is_current && self.agent_focus_surface_available(window, cx, false);
        let compact_drawer_contains_focus = terminal_focus_is_current
            && self
                .compact_drawer_focus_handle
                .contains_focused(window, cx);
        let focus_return = agent_terminal_exit_focus_return_target(
            lifecycle,
            exact_binding_is_current,
            terminal_focus_is_current,
            focus_surface_available,
            compact_drawer_contains_focus,
        );

        self.agent_terminal_lifecycle = Some(lifecycle);
        self.agent_needs_snapshot = false;
        self.clear_agent_creation_focus_for_terminal(session_id);
        if lifecycle_retains_terminal_history(lifecycle) {
            self.retire_pane_input_handler_for_history(session_id, window, cx);
        } else {
            self.invalidate_pane_input_handler(session_id, cx);
        }
        self.pending_agent_resizes.remove(session_id);
        self.pending_key_commits
            .retain(|pending| &pending.session_id != session_id);
        self.focused_agent_terminal = false;
        if self.reported_focus_session.as_ref() == Some(session_id) {
            self.reported_focus_session = None;
        }
        if let Some(panel) = self.agents_panel.as_ref() {
            panel.update(cx, |panel, cx| panel.refresh(cx));
        }
        match focus_return {
            Some(AgentTerminalExitFocusReturnTarget::CompactDrawer) => {
                window.focus(&self.compact_drawer_focus_handle, cx);
            }
            Some(AgentTerminalExitFocusReturnTarget::Workspace) => {
                window.focus(&self.focus_handle, cx);
            }
            None => {}
        }
        cx.notify();
    }

    /// 选择只替换订阅和画布投影；折叠、重新显示与普通 Tab 切换不重启 CLI 或修改绑定。
    ///
    /// @author kongweiguang
    pub(super) fn select_agent_terminal(
        &mut self,
        agent_session_id: Option<AgentSessionId>,
        session_id: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent_session_id == agent_session_id
            && self.agent_terminal_session_id == session_id
        {
            return;
        }
        self.agent_watch_cancellation.cancel();
        self.agent_watch_cancellation = CancellationToken::new();
        self.agent_watch_generation = self.agent_watch_generation.wrapping_add(1);
        if let Some(previous) = self.agent_terminal_session_id.clone() {
            self.invalidate_pane_input_handler(&previous, cx);
            self.pending_agent_resizes.remove(&previous);
        }
        if let Some(tools) = self.agent_terminal_tools.as_ref() {
            self.terminal_tools_runtime.cancel_source(tools.entity_id());
        }
        self.selected_agent_session_id = agent_session_id;
        self.agent_terminal_session_id = session_id.clone();
        self.agent_terminal_snapshot = None;
        self.agent_terminal_size = None;
        self.agent_terminal_lifecycle = None;
        self.agent_needs_snapshot = false;
        self.focused_agent_terminal = false;
        self.right_dock_collapsed = false;
        self.pending_key_commits.clear();
        if let Some(tools) = self.agent_terminal_tools.as_ref() {
            self.configure_terminal_tools(tools, session_id.clone(), window, cx);
        }
        self.refresh_context_panel_projection(cx);
        self.set_reported_focus_session(self.focused_terminal_session_id(), cx);
        self.cancel_unfocused_pane_compositions(window, cx);
        self.refresh_commands_session(cx);
        if let (Some(agent_session_id), Some(session_id)) =
            (self.selected_agent_session_id.clone(), session_id)
        {
            self.agent_needs_snapshot = true;
            self.start_agent_terminal_watch(agent_session_id, session_id, cx);
        }
        cx.notify();
    }

    /// 先订阅并同步当前 palette；终态更新回到所属 Window 线程，以便只在旧 Agent 仍聚焦时归还父级焦点。
    /// Exited 仅保留快照订阅，关闭、失败及其它非运行态停止。
    /// 搜索重查跟随已接受的新网格，覆盖订阅先于显式补帧到达的路径；普通输出不形成搜索回路。
    ///
    /// @author kongweiguang
    fn start_agent_terminal_watch(
        &mut self,
        agent_session_id: AgentSessionId,
        session_id: SessionId,
        cx: &mut Context<Self>,
    ) {
        let generation = self.agent_watch_generation;
        let cancellation = self.agent_watch_cancellation.clone();
        let services = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let subscribe_services = services.clone();
            let subscribe_id = session_id.clone();
            let subscribed = executor
                .run(move |_| async move { subscribe_services.subscribe(subscribe_id).await })
                .await;
            let mut subscription = match subscribed {
                Ok(subscription) => subscription,
                Err(error) => {
                    let window_root = this.clone();
                    let _ = cx.update_window(window_handle, move |_, window, app| {
                        let _ = window_root.update(app, |root, cx| {
                            if !root.agent_terminal_watch_is_current(
                                generation,
                                &agent_session_id,
                                &session_id,
                            ) {
                                return;
                            }
                            if error.public_code() == PublicErrorCode::NotFound {
                                root.complete_agent_terminal_watch(
                                    SessionLifecycle::Closed,
                                    &agent_session_id,
                                    &session_id,
                                    window,
                                    cx,
                                );
                            } else {
                                root.report_agent_terminal_failure(
                                    &agent_session_id,
                                    error.public_message().into(),
                                    cx,
                                );
                            }
                        });
                    });
                    return;
                }
            };
            let mut refresh_lifecycle = true;
            let mut request = Some(RenderSnapshotRequest::default());
            let mut applied_palette_generation = None;
            loop {
                if refresh_lifecycle || request.is_some() {
                    let palette_target = match this.update(cx, |root, _| {
                        root.agent_terminal_watch_is_current(
                            generation,
                            &agent_session_id,
                            &session_id,
                        )
                        .then(|| AgentTerminalPaletteTarget {
                            palette: root.current_terminal_palette(),
                            generation: root.palette_update_generation,
                        })
                    }) {
                        Ok(Some(target)) => target,
                        _ => return,
                    };
                    let apply_palette = agent_palette_update_required(
                        applied_palette_generation,
                        palette_target.generation,
                    );
                    let frame_services = services.clone();
                    let frame_agent = agent_session_id.clone();
                    let frame_id = session_id.clone();
                    let frame_request = request.take().unwrap_or_default();
                    let frame_palette = apply_palette.then_some(palette_target.palette);
                    let frame = executor
                        .run(move |_| async move {
                            read_agent_terminal_frame(
                                frame_services,
                                frame_agent,
                                frame_id,
                                frame_request,
                                refresh_lifecycle || apply_palette,
                                frame_palette,
                            )
                            .await
                        })
                        .await;
                    let palette_applied = apply_palette && frame.is_ok();
                    let snapshot_palette_matches = frame
                        .as_ref()
                        .ok()
                        .and_then(|frame| frame.snapshot.as_ref())
                        .map(|snapshot| snapshot.palette == palette_target.palette);
                    let input_context = frame
                        .as_ref()
                        .ok()
                        .and_then(|frame| frame.input_context.clone());
                    let frame_failed = matches!(&frame, Ok(frame) if frame.failure.is_some());
                    let frame_root = this.clone();
                    let progress = cx
                        .update_window(window_handle, |_, window, app| {
                            frame_root.update(app, |root, cx| {
                                match agent_frame_acceptance(
                                    root.agent_terminal_watch_is_current(
                                        generation,
                                        &agent_session_id,
                                        &session_id,
                                    ),
                                    palette_target.generation,
                                    root.palette_update_generation,
                                    snapshot_palette_matches,
                                ) {
                                    AgentFrameAcceptance::StopStaleWatch => {
                                        return AgentWatchProgress::Stop;
                                    }
                                    AgentFrameAcceptance::RetryWithCurrentPalette => {
                                        root.agent_needs_snapshot = true;
                                        cx.notify();
                                        return AgentWatchProgress::RetryWithCurrentPalette;
                                    }
                                    AgentFrameAcceptance::Accept => {}
                                }
                                if palette_applied {
                                    applied_palette_generation = Some(palette_target.generation);
                                }
                                match frame {
                                    Ok(frame) => {
                                        if let Some(snapshot) = frame.snapshot
                                            && root.agent_terminal_snapshot.as_ref().is_none_or(
                                                |current| snapshot.revision >= current.revision,
                                            )
                                        {
                                            let view_grid_changed = root
                                                .agent_terminal_snapshot
                                                .as_ref()
                                                .is_none_or(|current| {
                                                    current.size != snapshot.size
                                                        || current.grid_epoch != snapshot.grid_epoch
                                                });
                                            root.agent_terminal_size = Some(snapshot.size);
                                            root.agent_terminal_snapshot = Some(snapshot);
                                            root.agent_needs_snapshot = false;
                                            if view_grid_changed {
                                                root.refresh_open_terminal_search_for_view_revision(
                                                    &session_id,
                                                    cx,
                                                );
                                            }
                                        }
                                        if let Some(lifecycle) = frame.lifecycle {
                                            let previous_lifecycle = root.agent_terminal_lifecycle;
                                            root.agent_terminal_lifecycle = Some(lifecycle);
                                            if lifecycle != SessionLifecycle::Running {
                                                if lifecycle != SessionLifecycle::Exited
                                                    || previous_lifecycle
                                                        != Some(SessionLifecycle::Exited)
                                                {
                                                    root.complete_agent_terminal_watch(
                                                        lifecycle,
                                                        &agent_session_id,
                                                        &session_id,
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            } else if frame.input_context.is_none() {
                                                root.clear_agent_creation_focus_for_terminal(
                                                    &session_id,
                                                );
                                                root.invalidate_pane_input_handler(&session_id, cx);
                                                root.pane_input_binding_failures
                                                    .insert(session_id.clone());
                                            }
                                        }
                                        if let Some(error) = frame.failure {
                                            root.report_agent_terminal_failure(
                                                &agent_session_id,
                                                error.public_message().into(),
                                                cx,
                                            );
                                        }
                                        if root.agent_terminal_lifecycle.is_none_or(|lifecycle| {
                                            !agent_lifecycle_allows_snapshot(lifecycle)
                                        }) || frame_failed
                                        {
                                            return AgentWatchProgress::Stop;
                                        }
                                    }
                                    Err(error) => {
                                        root.report_agent_terminal_failure(
                                            &agent_session_id,
                                            error.public_message().into(),
                                            cx,
                                        );
                                        cx.notify();
                                        return AgentWatchProgress::Stop;
                                    }
                                }
                                if !root.right_dock_collapsed
                                    && preferences::interaction_policy(root).is_some_and(|policy| {
                                        policy.command_block_rail_visible(true)
                                    })
                                    && let Some(tools) = root.agent_terminal_tools.clone()
                                {
                                    command_blocks::refresh_from_event(
                                        root,
                                        tools,
                                        session_id.clone(),
                                        cx,
                                    );
                                }
                                cx.notify();
                                AgentWatchProgress::Continue
                            })
                        })
                        .ok()
                        .and_then(|progress| progress.ok())
                        .unwrap_or(AgentWatchProgress::Stop);
                    match progress {
                        AgentWatchProgress::Stop => return,
                        AgentWatchProgress::RetryWithCurrentPalette => {
                            applied_palette_generation = None;
                            refresh_lifecycle = true;
                            request = Some(RenderSnapshotRequest::default());
                            continue;
                        }
                        AgentWatchProgress::Continue => {}
                    }
                    if let Some(context) = input_context {
                        let window_root = this.clone();
                        let window_agent = agent_session_id.clone();
                        let window_session = session_id.clone();
                        let _ = cx.update_window(window_handle, move |_, window, app| {
                            let _ = window_root.update(app, |root, cx| {
                                if root.agent_terminal_watch_is_current(
                                    generation,
                                    &window_agent,
                                    &window_session,
                                ) && root.agent_terminal_lifecycle
                                    == Some(SessionLifecycle::Running)
                                {
                                    let binding = UiSessionBinding {
                                        session_id: context.session_id.clone(),
                                        owner: context.owner.clone(),
                                        session_generation: context.session_generation,
                                    };
                                    root.install_pane_input_handler(binding, context, window, cx);
                                    root.try_consume_agent_creation_focus_intent(
                                        generation,
                                        &window_agent,
                                        &window_session,
                                        window,
                                        cx,
                                    );
                                }
                            });
                        });
                    }
                }
                let receive_cancellation = cancellation.clone();
                let received = executor
                    .run(move |_| async move {
                        let change = subscription.recv(&receive_cancellation).await;
                        Ok((subscription, change))
                    })
                    .await;
                let (next_subscription, change) = match received {
                    Ok(received) => received,
                    Err(error) => {
                        let _ = this.update(cx, |root, cx| {
                            if root.agent_terminal_watch_is_current(
                                generation,
                                &agent_session_id,
                                &session_id,
                            ) {
                                root.report_agent_terminal_failure(
                                    &agent_session_id,
                                    error.public_message().into(),
                                    cx,
                                );
                            }
                        });
                        return;
                    }
                };
                subscription = next_subscription;
                let change = match change {
                    Ok(Some(change)) => change,
                    Err(_) if cancellation.is_cancelled() => return,
                    Err(error) => {
                        let _ = this.update(cx, |root, cx| {
                            if root.agent_terminal_watch_is_current(
                                generation,
                                &agent_session_id,
                                &session_id,
                            ) {
                                root.report_agent_terminal_failure(
                                    &agent_session_id,
                                    error.public_message().into(),
                                    cx,
                                );
                            }
                        });
                        return;
                    }
                    Ok(None) => {
                        let window_root = this.clone();
                        let _ = cx.update_window(window_handle, move |_, window, app| {
                            let _ = window_root.update(app, |root, cx| {
                                if root.agent_terminal_watch_is_current(
                                    generation,
                                    &agent_session_id,
                                    &session_id,
                                ) {
                                    root.complete_agent_terminal_watch(
                                        SessionLifecycle::Closed,
                                        &agent_session_id,
                                        &session_id,
                                        window,
                                        cx,
                                    );
                                }
                            });
                        });
                        return;
                    }
                };
                if change.kind == SessionChangeKind::Closed {
                    let window_root = this.clone();
                    let _ = cx.update_window(window_handle, move |_, window, app| {
                        let _ = window_root.update(app, |root, cx| {
                            if root.agent_terminal_watch_is_current(
                                generation,
                                &agent_session_id,
                                &session_id,
                            ) {
                                root.complete_agent_terminal_watch(
                                    SessionLifecycle::Closed,
                                    &agent_session_id,
                                    &session_id,
                                    window,
                                    cx,
                                );
                            }
                        });
                    });
                    return;
                }
                refresh_lifecycle = change.kind == SessionChangeKind::Lifecycle;
                request = match this.update(cx, |root, _| {
                    if !root.agent_terminal_watch_is_current(
                        generation,
                        &agent_session_id,
                        &session_id,
                    ) {
                        return None;
                    }
                    root.agent_needs_snapshot = true;
                    Some((!root.right_dock_collapsed).then(|| {
                        let current = root.agent_terminal_snapshot.as_ref();
                        RenderSnapshotRequest {
                            since_revision: current.map(|snapshot| snapshot.revision),
                        }
                    }))
                }) {
                    Ok(Some(request)) => request,
                    _ => return,
                };
            }
        })
        .detach();
    }

    /// 重新显示只补取隐藏期间的画布；迟到帧必须匹配当前 palette 与 watcher，NotFound 在对应窗口线程收敛关闭并归还旧终端焦点。
    /// 同代次但实际配色仍旧时保留脏标记，交由已订阅的配色变更刷新，避免 render 通知形成即时重试环。
    /// 新网格接受后重查当前搜索，不能在 resize ACK 时使用旧尺寸坐标，也不对普通输出快照重复重搜。
    ///
    /// @author kongweiguang
    pub(super) fn request_agent_snapshot(&mut self, cx: &mut Context<Self>) {
        let (Some(agent_session_id), Some(session_id)) = (
            self.selected_agent_session_id.clone(),
            self.agent_terminal_session_id.clone(),
        ) else {
            return;
        };
        if self.right_dock_collapsed
            || !self.agent_needs_snapshot
            || self
                .agent_terminal_lifecycle
                .is_none_or(|lifecycle| !agent_lifecycle_allows_snapshot(lifecycle))
        {
            return;
        }
        self.agent_needs_snapshot = false;
        let palette_target = AgentTerminalPaletteTarget {
            palette: self.current_terminal_palette(),
            generation: self.palette_update_generation,
        };
        let generation = self.agent_watch_generation;
        let request = self
            .agent_terminal_snapshot
            .as_ref()
            .map(|snapshot| RenderSnapshotRequest {
                since_revision: Some(snapshot.revision),
            })
            .unwrap_or_default();
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let render_id = session_id.clone();
            let result = executor
                .run(move |_| async move { sessions.render_snapshot(render_id, request).await })
                .await;
            let snapshot_palette_matches = result
                .as_ref()
                .ok()
                .map(|snapshot| snapshot.palette == palette_target.palette);
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    let watch_is_current = root.agent_terminal_watch_is_current(
                        generation,
                        &agent_session_id,
                        &session_id,
                    ) && root
                        .agent_terminal_lifecycle
                        .is_some_and(agent_lifecycle_allows_snapshot);
                    match agent_frame_acceptance(
                        watch_is_current,
                        palette_target.generation,
                        root.palette_update_generation,
                        snapshot_palette_matches,
                    ) {
                        AgentFrameAcceptance::StopStaleWatch => return,
                        AgentFrameAcceptance::RetryWithCurrentPalette => {
                            root.agent_needs_snapshot = true;
                            // 代次变化意味着 Root 已切到另一主题，调度一次当前帧；同代次的旧 palette 会由 set_palette 的订阅事件刷新。
                            if palette_target.generation != root.palette_update_generation {
                                cx.notify();
                            }
                            return;
                        }
                        AgentFrameAcceptance::Accept => {}
                    }
                    match result {
                        Ok(snapshot)
                            if root
                                .agent_terminal_snapshot
                                .as_ref()
                                .is_none_or(|current| snapshot.revision >= current.revision) =>
                        {
                            let view_grid_changed =
                                root.agent_terminal_snapshot.as_ref().is_none_or(|current| {
                                    current.size != snapshot.size
                                        || current.grid_epoch != snapshot.grid_epoch
                                });
                            root.agent_terminal_size = Some(snapshot.size);
                            root.agent_terminal_snapshot = Some(snapshot);
                            if view_grid_changed {
                                root.refresh_open_terminal_search_for_view_revision(
                                    &session_id,
                                    cx,
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(error) if error.public_code() == PublicErrorCode::NotFound => {
                            root.complete_agent_terminal_watch(
                                SessionLifecycle::Closed,
                                &agent_session_id,
                                &session_id,
                                window,
                                cx,
                            );
                        }
                        Err(error) => {
                            root.agent_needs_snapshot = true;
                            root.report_agent_terminal_failure(
                                &agent_session_id,
                                error.public_message().into(),
                                cx,
                            );
                            return;
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentTerminalExitFocusReturnTarget {
    CompactDrawer,
    Workspace,
}

/// 只在同一终态 Agent PTY 仍真实聚焦且面板可用时归还父级焦点；退出中的旧 watcher 不能抢焦点。
/// 后台 Window::focus 仅更新窗口内焦点，不激活 OS 窗口；紧凑抽屉保留其 Escape/快捷键接收者。
///
/// @author kongweiguang
fn agent_terminal_exit_focus_return_target(
    lifecycle: SessionLifecycle,
    exact_binding_is_current: bool,
    terminal_focus_is_current: bool,
    focus_surface_available: bool,
    compact_drawer_contains_focus: bool,
) -> Option<AgentTerminalExitFocusReturnTarget> {
    if !matches!(
        lifecycle,
        SessionLifecycle::Exited | SessionLifecycle::Closed | SessionLifecycle::Failed
    ) || !exact_binding_is_current
        || !terminal_focus_is_current
        || !focus_surface_available
    {
        return None;
    }
    Some(if compact_drawer_contains_focus {
        AgentTerminalExitFocusReturnTarget::CompactDrawer
    } else {
        AgentTerminalExitFocusReturnTarget::Workspace
    })
}

/// compact 主机与工具复用物理句柄，因此焦点相等不能替代用户导航代次；两个条件必须同时成立。
///
/// @author kongweiguang
fn agent_creation_focus_source_is_current(
    requested_generation: u64,
    current_generation: u64,
    focus_unchanged: bool,
) -> bool {
    requested_generation == current_generation && focus_unchanged
}

#[cfg(test)]
#[path = "../../tests/unit/root_agent_terminal.rs"]
mod tests;
