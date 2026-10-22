//! 按会话 FIFO 调度终端输入与控制命令，并在真实 ACK 后推进界面状态。
//!
//! @author kongweiguang

use super::{
    CachedTerminalCommandContext, CenterPanel, RootView, SessionCommand, command_line_projection,
    command_suggestions, session_queue,
    session_reconnect::SessionPanelObservation,
    workspace_runtime::{RuntimePanePhase, WorkspaceRuntime},
};
use gpui_kit::Context;
use kerminal_application::{
    AppResult,
    sessions::{
        ScrollRequest, TerminalCommandAvailability, TerminalCommandContext,
        TerminalCommandUnavailableReason, UiInputProjection, UiSessionBinding,
        UiTerminalCommandSubmissionRequest, UiTerminalInputRequest,
        command_blocks::is_plain_command_enter,
    },
};
use kerminal_domain::{
    commands::SuggestionQueryMode,
    identity::{AgentSessionId, SessionId},
    sessions::{SessionOwner, SessionTarget, TerminalSize},
};
use kerminal_terminal::{input::TerminalInputEvent, snapshot::RenderSnapshotRequest};

/// 准入拒绝或失败回执只抑制同一 session 的同一尺寸；改变布局后允许新尝试，不建立重绘重试循环。
/// 绑定失效时由 Root 清理条目，旧 session 的失败不会阻断重连或其它 pane。
///
/// @author kongweiguang
pub(super) fn allow_canvas_resize_after_failure(
    failures: &mut std::collections::HashMap<SessionId, TerminalSize>,
    session_id: &SessionId,
    size: TerminalSize,
) -> bool {
    match failures.get(session_id) {
        Some(failed_size) if *failed_size == size => false,
        Some(_) => {
            failures.remove(session_id);
            true
        }
        None => true,
    }
}

/// 只有同一 binding 的权威观测能证明 resize 目标已结束；缺失或错代观测保留原有错误路径。
///
/// @author kongweiguang
pub(super) fn observation_proves_session_not_running(
    binding: &UiSessionBinding,
    observation: Option<&SessionPanelObservation>,
) -> bool {
    observation.is_some_and(|observation| {
        observation.binding == *binding
            && observation.lifecycle != kerminal_domain::sessions::SessionLifecycle::Running
    })
}

/// 仅吞掉精确 UI binding 已被 pane、目标和 generation 观测为 ended 后的迟到 Focus 回执。
/// 普通输入、缺失观测、Running 状态或任何错代结果都继续显示真实 writer 错误。
///
/// @author kongweiguang
pub(super) fn focus_failure_is_proven_ended(
    request: &UiTerminalInputRequest,
    receipt_binding: Option<&UiSessionBinding>,
    target: &SessionTarget,
    observation: Option<&SessionPanelObservation>,
) -> bool {
    let Some(receipt_binding) = receipt_binding else {
        return false;
    };
    request.binding == *receipt_binding
        && matches!(
            &request.event,
            kerminal_terminal::input::TerminalInputEvent::Focus(_)
        )
        && observation.is_some_and(|observation| observation.target == *target)
        && observation_proves_session_not_running(receipt_binding, observation)
}

/// 只从当前 owner 对应的 pane 读取同目标服务观测，供 resize 与焦点回执共用同一身份门禁。
///
/// @author kongweiguang
fn panel_observation_for_receipt(
    root: &RootView,
    receipt: &SessionCommandReceipt,
) -> Option<(SessionTarget, SessionPanelObservation)> {
    let binding = receipt.binding.as_ref()?;
    let SessionOwner::Ui { tab_id, pane_id } = &receipt.owner else {
        return None;
    };
    let pane = root.workspace_runtime.pane(pane_id)?;
    if pane.tab_id != *tab_id || pane.session_id.as_ref() != Some(&receipt.session_id) {
        return None;
    }
    let target = pane.target.as_ref()?;
    let observation =
        root.session_reconnect
            .panel_observation(pane_id, tab_id, &receipt.session_id, target)?;
    (observation.binding == *binding && observation.target == *target)
        .then(|| (target.clone(), observation))
}

/// 队列项派发时固定真实 owner 与已知 binding，回执不再跟随活动 Tab 或物理焦点。
struct SessionCommandReceipt {
    session_id: SessionId,
    owner: SessionOwner,
    binding: Option<UiSessionBinding>,
    binding_source: SessionCommandBindingSource,
    selection_scroll: Option<SelectionScrollReceipt>,
}

/// 边缘滚动回执固定派发时的手势与请求代次，不能被后续拖选覆盖。
#[derive(Clone)]
struct SelectionScrollReceipt {
    binding: UiSessionBinding,
    gesture_id: u64,
    request_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionCommandBindingSource {
    InputHandler,
    InputHandlerAndView,
    ReadOnlyView,
}

impl SessionCommandReceipt {
    /// 输入与运行态 resize 沿用 live binding；视口滚动使用只读 view binding并保留边缘手势回执身份。
    ///
    /// @author kongweiguang
    fn capture(
        ready: &session_queue::ReadySessionWork,
        owner: SessionOwner,
        current_binding: Option<&UiSessionBinding>,
    ) -> Self {
        let (binding, binding_source, selection_scroll) = match &ready.work {
            session_queue::SessionWork::UiInput(work) => (
                Some(work.request.binding.clone()),
                SessionCommandBindingSource::InputHandler,
                None,
            ),
            session_queue::SessionWork::Scroll { binding, .. }
            | session_queue::SessionWork::ScrollTo { binding, .. }
            | session_queue::SessionWork::HistoryResize { binding, .. } => (
                Some(binding.clone()),
                SessionCommandBindingSource::ReadOnlyView,
                None,
            ),
            session_queue::SessionWork::SelectionScroll {
                binding,
                gesture_id,
                request_id,
                ..
            } => (
                Some(binding.clone()),
                SessionCommandBindingSource::ReadOnlyView,
                Some(SelectionScrollReceipt {
                    binding: binding.clone(),
                    gesture_id: *gesture_id,
                    request_id: *request_id,
                }),
            ),
            session_queue::SessionWork::Resize { binding, .. } => (
                Some(binding.clone()),
                SessionCommandBindingSource::InputHandlerAndView,
                None,
            ),
            session_queue::SessionWork::Input { .. } => (
                current_binding.cloned(),
                SessionCommandBindingSource::InputHandler,
                None,
            ),
        };
        Self {
            session_id: ready.session_id.clone(),
            owner,
            binding,
            binding_source,
            selection_scroll,
        }
    }
}

/// 可见性不是 owner 生命周期；仅真实关闭、移除或当前 Agent 视图换绑才撤销该路由。
///
/// @author kongweiguang
fn session_command_owner(
    runtime: &WorkspaceRuntime,
    session_id: &SessionId,
    agent_session_id: Option<&AgentSessionId>,
    agent_terminal_session_id: Option<&SessionId>,
) -> Option<SessionOwner> {
    if let Some(pane_id) = runtime.pane_for_session(session_id) {
        let pane = runtime.pane(pane_id)?;
        return (pane.phase == RuntimePanePhase::Ready
            && pane.session_id.as_ref() == Some(session_id))
        .then(|| SessionOwner::Ui {
            tab_id: pane.tab_id.clone(),
            pane_id: pane_id.clone(),
        });
    }
    (agent_terminal_session_id == Some(session_id))
        .then_some(agent_session_id)
        .flatten()
        .map(|agent_session_id| SessionOwner::Agent {
            agent_session_id: agent_session_id.clone(),
        })
}

/// Extends owner lookup for view-only work while requiring a naturally retired CloseFailed ticket.
///
/// @author kongweiguang
fn session_command_view_owner(
    runtime: &WorkspaceRuntime,
    session_id: &SessionId,
    agent_session_id: Option<&AgentSessionId>,
    agent_terminal_session_id: Option<&SessionId>,
) -> Option<SessionOwner> {
    if let Some(pane_id) = runtime.pane_for_session(session_id) {
        let pane = runtime.pane(pane_id)?;
        if pane.phase == RuntimePanePhase::CloseFailed {
            let binding = pane.retired_history_binding.as_ref()?;
            return runtime
                .close_failed_history_binding_matches(pane_id, binding)
                .then(|| binding.owner.clone());
        }
    }
    session_command_owner(
        runtime,
        session_id,
        agent_session_id,
        agent_terminal_session_id,
    )
}

/// 只读滚动/历史重排只依赖 view binding；运行态 resize 同时要求 live 与 view binding 相同。
///
/// @author kongweiguang
fn session_work_binding_is_current(
    work: &session_queue::SessionWork,
    input_binding: Option<&UiSessionBinding>,
    view_binding: Option<&UiSessionBinding>,
) -> bool {
    match work {
        session_queue::SessionWork::Scroll { binding, .. }
        | session_queue::SessionWork::ScrollTo { binding, .. }
        | session_queue::SessionWork::SelectionScroll { binding, .. }
        | session_queue::SessionWork::HistoryResize { binding, .. } => {
            view_binding == Some(binding)
        }
        session_queue::SessionWork::Resize { binding, .. } => {
            input_binding == Some(binding) && view_binding == Some(binding)
        }
        session_queue::SessionWork::UiInput(work) => input_binding == Some(&work.request.binding),
        session_queue::SessionWork::Input { .. } => input_binding.is_some(),
    }
}

/// 先判定派发 owner 和 binding 是否仍有效，再保留原始结果；不按错误码吞掉当前 owner 的失败。
///
/// @author kongweiguang
fn accept_session_command_result(
    receipt: &SessionCommandReceipt,
    current_owner: Option<SessionOwner>,
    current_binding: Option<&UiSessionBinding>,
    result: AppResult<Option<TerminalCommandContext>>,
) -> Option<AppResult<Option<TerminalCommandContext>>> {
    if current_owner.as_ref() != Some(&receipt.owner)
        || receipt
            .binding
            .as_ref()
            .is_some_and(|binding| current_binding != Some(binding))
    {
        return None;
    }
    Some(result)
}

/// 将可信 command context 压成固定有限数字码；日志只保留状态类别，不格式化正文或上下文对象。
///
/// 0 表示可用，1..=11 对应当前 unavailable reason 枚举，255 表示没有匹配 binding 的缓存上下文。
/// @author kongweiguang
fn terminal_command_unavailable_reason_code(context: Option<&TerminalCommandContext>) -> i64 {
    let Some(context) = context else {
        return 255;
    };
    match context.availability {
        TerminalCommandAvailability::Available => 0,
        TerminalCommandAvailability::Unavailable(reason) => match reason {
            TerminalCommandUnavailableReason::NonUiOwner => 1,
            TerminalCommandUnavailableReason::SessionNotRunning => 2,
            TerminalCommandUnavailableReason::ShellIntegrationDisabled => 3,
            TerminalCommandUnavailableReason::ShellIntegrationNotObserved => 4,
            TerminalCommandUnavailableReason::PromptNotReady => 5,
            TerminalCommandUnavailableReason::AlternateScreen => 6,
            TerminalCommandUnavailableReason::UnsupportedShellDialect => 7,
            TerminalCommandUnavailableReason::ExternalInputObserved => 8,
            TerminalCommandUnavailableReason::InputWritePending => 9,
            TerminalCommandUnavailableReason::UiInputProjectionInvalidated => 10,
            TerminalCommandUnavailableReason::ManagedCommandActive => 11,
        },
    }
}

/// 只有会写入终端的按键、文本和粘贴会回到底部；焦点与鼠标报告不能改变阅读位置。
///
/// @author kongweiguang
fn input_returns_viewport_to_bottom(event: &TerminalInputEvent) -> bool {
    matches!(
        event,
        TerminalInputEvent::Key(_)
            | TerminalInputEvent::KeyWithText { .. }
            | TerminalInputEvent::Text(_)
            | TerminalInputEvent::Paste(_)
    )
}

/// 只有当前 UI pane 的完整 binding 匹配时才废弃旧 viewport 绘制回执。
///
/// @author kongweiguang
fn invalidate_workspace_snapshot_for_scroll(
    root: &mut RootView,
    binding: &UiSessionBinding,
) -> bool {
    let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
        return false;
    };
    let binding_is_current = root.terminal_view_bindings.get(&binding.session_id) == Some(binding);
    let pane_is_current = root.workspace_runtime.pane(pane_id).is_some_and(|pane| {
        pane.tab_id == *tab_id
            && pane.session_id.as_ref() == Some(&binding.session_id)
            && (pane.phase == RuntimePanePhase::Ready
                || root
                    .workspace_runtime
                    .close_failed_history_binding_matches(pane_id, binding))
    });
    if !binding_is_current || !pane_is_current {
        return false;
    }
    root.workspace_runtime
        .invalidate_snapshot_for_viewport_change(pane_id, &binding.session_id)
        .unwrap_or(false)
}

impl RootView {
    /// 焦点报告与队列共用实际 owner 判定；Agent 未 Running 时拒绝旧请求及迟到回执。
    ///
    /// @author kongweiguang
    pub(super) fn session_command_owner(&self, session_id: &SessionId) -> Option<SessionOwner> {
        if self.agent_terminal_session_id.as_ref() == Some(session_id)
            && self.agent_terminal_lifecycle
                != Some(kerminal_domain::sessions::SessionLifecycle::Running)
        {
            return None;
        }
        session_command_owner(
            &self.workspace_runtime,
            session_id,
            self.selected_agent_session_id.as_ref(),
            self.agent_terminal_session_id.as_ref(),
        )
    }

    /// 只为历史视口读取 owner，不应用 Agent 写入态门禁；精确 view binding 仍在队列与回执处验证。
    ///
    /// @author kongweiguang
    pub(super) fn session_command_view_owner(
        &self,
        session_id: &SessionId,
    ) -> Option<SessionOwner> {
        session_command_view_owner(
            &self.workspace_runtime,
            session_id,
            self.selected_agent_session_id.as_ref(),
            self.agent_terminal_session_id.as_ref(),
        )
    }

    /// 只用真实 FIFO ACK 推进建议和命令行投影；后续 CAS work 各自单次装箱并按 ACK 串行入队。
    /// 焦点报告只刷新真实上下文，不把无编辑语义的通知交给正文/建议编辑器。
    fn complete_ui_input_ack(
        &mut self,
        request: UiTerminalInputRequest,
        context: TerminalCommandContext,
        acceptance: Option<session_queue::SuggestionAcceptanceWrite>,
        command_line_ticket: Option<command_line_projection::CommandLineInputTicket>,
        command_submitted: bool,
        cx: &mut Context<Self>,
    ) {
        if context.session_id != request.binding.session_id
            || context.owner != request.binding.owner
            || context.session_generation != request.binding.session_generation
            || !self
                .pane_input_handlers
                .get(&request.binding.session_id)
                .is_some_and(|registration| registration.binding == request.binding)
        {
            if let Some(ticket) = command_line_ticket.as_ref() {
                self.command_line_projection.apply_failure(ticket);
            }
            if let Some(write) = acceptance {
                self.terminal_command_suggestions
                    .acceptance_input_failed(&write.acceptance, write.event_index);
            }
            if self
                .suggestion_menu_binding
                .as_ref()
                .is_some_and(|binding| binding == &request.binding)
            {
                self.terminal_command_suggestions.clear();
                self.suggestion_menu_binding = None;
                self.sync_suggestion_menu(cx);
            }
            return;
        }

        if let Some(ticket) = command_line_ticket.as_ref() {
            if command_submitted {
                self.command_line_projection
                    .apply_submission_ack(ticket, &request, &context);
            } else {
                self.command_line_projection
                    .apply_ack(ticket, &request, &context);
            }
        } else if acceptance.is_some() {
            // 建议替换由自己的 CAS 计划驱动，Root 不把多事件替换伪装成逐键行编辑。
            self.command_line_projection
                .invalidate_binding(&request.binding);
        }
        self.command_line_projection
            .observe_context(&request.binding, &context);

        self.terminal_command_contexts.insert(
            request.binding.session_id.clone(),
            CachedTerminalCommandContext {
                binding: request.binding.clone(),
                context: context.clone(),
            },
        );

        if matches!(
            request.event,
            kerminal_terminal::input::TerminalInputEvent::Focus(_)
        ) {
            return;
        }

        if let Some(write) = acceptance {
            if !self.is_active_suggestion_binding(&request.binding) {
                self.terminal_command_suggestions
                    .acceptance_input_failed(&write.acceptance, write.event_index);
                self.suggestion_menu_binding = None;
                self.sync_suggestion_menu(cx);
                return;
            }
            match self.terminal_command_suggestions.acceptance_input_written(
                &write.acceptance,
                write.event_index,
                &context,
                write.mode,
            ) {
                command_suggestions::AcceptanceInputProgress::Next { event_index } => {
                    let next = self.terminal_command_suggestions.acceptance_input_request(
                        &write.acceptance,
                        event_index,
                        &context,
                    );
                    match next {
                        Some(request) => {
                            let work = session_queue::SessionWork::UiInput(Box::new(
                                session_queue::UiInputWork::new(
                                    request,
                                    Some(session_queue::SuggestionAcceptanceWrite {
                                        acceptance: write.acceptance.clone(),
                                        event_index,
                                        mode: write.mode,
                                    }),
                                ),
                            ));
                            self.command_line_projection
                                .invalidate_binding(&write.acceptance.binding);
                            if self
                                .session_commands
                                .enqueue_front(write.acceptance.binding.session_id.clone(), work)
                                .is_err()
                            {
                                self.terminal_command_suggestions
                                    .acceptance_input_failed(&write.acceptance, event_index);
                                self.status = Some(
                                    "终端输入队列已满，建议替换已停止；未发送剩余编辑。".into(),
                                );
                            }
                        }
                        None => {
                            self.terminal_command_suggestions
                                .acceptance_input_failed(&write.acceptance, event_index);
                        }
                    }
                }
                command_suggestions::AcceptanceInputProgress::Complete { follow_up } => {
                    if let Some(job) = follow_up {
                        self.run_terminal_suggestion_job(job, cx);
                    }
                }
                command_suggestions::AcceptanceInputProgress::Invalidated => {}
            }
            self.sync_suggestion_menu(cx);
            return;
        }

        if request.projection == UiInputProjection::Tracked
            && self.is_active_suggestion_binding(&request.binding)
        {
            let mode = if self.terminal_command_suggestions.menu_is_open() {
                SuggestionQueryMode::Menu
            } else {
                SuggestionQueryMode::Inline
            };
            let job = self.terminal_command_suggestions.tracked_input_written(
                &context,
                &request.event,
                mode,
            );
            if self.terminal_command_suggestions.menu_is_open() {
                self.suggestion_menu_binding = Some(request.binding.clone());
            }
            self.sync_suggestion_menu(cx);
            if let Some(job) = job {
                self.run_terminal_suggestion_job(job, cx);
            }
        } else if self.is_active_suggestion_binding(&request.binding) {
            self.terminal_command_suggestions.clear();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
        }
    }

    /// 一个固定 binding 的广播目标拒绝 CAS 后，停止其余待发项并同步清除工具视图的活动态。
    fn fail_broadcast_input(&mut self, generation: u64, message: String, cx: &mut Context<Self>) {
        if self.broadcast_generation != generation || self.broadcast_session_ids.is_none() {
            return;
        }
        let dropped = self.stop_broadcast();
        let message = crate::locale::text(
            format!(
                "广播目标拒绝了当前绑定，已停止广播并撤销 {} 条待发送输入。{}",
                dropped.broadcast_events, message
            ),
            format!(
                "A broadcast target rejected its captured binding. Broadcast stopped and {} queued events were cancelled. {}",
                dropped.broadcast_events, message
            ),
        )
        .to_string();
        self.status = Some(message.clone().into());
        let tools = self
            .workspace_tools
            .values()
            .cloned()
            .chain(self.agent_terminal_tools.clone())
            .collect::<Vec<_>>();
        for tools in tools {
            let message = message.clone();
            tools.update(cx, |tools, cx| {
                tools.set_broadcast_active(false, cx);
                tools.set_broadcast_pending(false, cx);
                tools.set_broadcast_error(message, cx);
            });
        }
        cx.notify();
    }

    /// 从按会话 FIFO 启动有限并发任务；SelectionScroll 以真实快照 revision 回 ACK，避免旧画面确认新滚动。
    /// UiInput work 在执行端解箱，避免重复复制可能较大的输入正文。
    /// 运行态 PTY resize 只有成功 ACK 才更新确认尺寸，失败后抑制同尺寸的重绘重试。
    /// 搜索坐标绑定渲染 revision；失败补偿保留快照时重查，成功重排交由新快照接受后刷新。
    /// Exited/Failed 的 pane 用只读 view binding 滚动和重排历史；回执只更新 view grid，不记作 PTY ACK。
    /// 可靠的 tracked Enter 保留原命令块 writer；历史在它返回 ACK 后独立追加，失败不回滚或重放输入。
    /// 普通 Enter 仅输出静态诊断码与有界状态数字，不记录命令、字符、目标标识或上下文 Debug。
    /// 回执固定派发时的视图 owner；换 Agent 视图不撤销已受理的后台工作，只拒收旧视图结果。
    /// 关闭通过独立门禁撤销待发项，已经派发的旧结果只释放 FIFO 槽位，不覆盖当前全局状态。
    /// 持续广播项若在出队时发现固定 binding 失效，按捕获代次停止并反馈，不能静默丢弃或影响新代。
    ///
    /// @author kongweiguang
    fn start_session_command_pump(&mut self, cx: &mut Context<Self>) {
        while let Some(ready) = self.session_commands.take_ready() {
            #[cfg(feature = "acceptance-metrics")]
            let echo_probe_id = match &ready.work {
                session_queue::SessionWork::UiInput(work) => work.echo_probe_id.clone(),
                _ => None,
            };
            #[cfg(feature = "acceptance-metrics")]
            if let Some(probe_id) = echo_probe_id.as_deref() {
                crate::acceptance_metrics::terminal_echo_probe_registry().record_phase_for_probe(
                    probe_id,
                    crate::acceptance_metrics::TerminalEchoPhase::FifoDequeued,
                    None,
                );
            }
            let input_binding = self
                .pane_input_handlers
                .get(&ready.session_id)
                .map(|registration| &registration.binding);
            let view_binding = self.terminal_view_bindings.get(&ready.session_id);
            let is_read_only_view_work = matches!(
                &ready.work,
                session_queue::SessionWork::Scroll { .. }
                    | session_queue::SessionWork::ScrollTo { .. }
                    | session_queue::SessionWork::SelectionScroll { .. }
                    | session_queue::SessionWork::HistoryResize { .. }
            );
            let owner = if is_read_only_view_work {
                self.session_command_view_owner(&ready.session_id)
            } else {
                self.session_command_owner(&ready.session_id)
            };
            let receipt = owner
                .filter(|_| {
                    session_work_binding_is_current(&ready.work, input_binding, view_binding)
                })
                .map(|owner| SessionCommandReceipt::capture(&ready, owner, input_binding));
            let Some(receipt) = receipt else {
                // 持续广播的 binding 失效必须停掉同代剩余目标并反馈；代次门禁保护新一轮广播。
                if let session_queue::SessionWork::UiInput(work) = &ready.work
                    && let session_queue::InputOrigin::Broadcast(generation) = work.origin
                {
                    self.fail_broadcast_input(
                        generation,
                        "固定广播目标已失效，当前输入未发送。".to_owned(),
                        cx,
                    );
                }
                // 其它退出或换代 work 只释放 FIFO 槽位，绝不尝试旧写入或旧滚动。
                self.session_commands.complete(&ready.session_id);
                continue;
            };
            let receipt = Some(receipt);
            let session_input_binding =
                receipt.as_ref().and_then(|receipt| receipt.binding.clone());
            let viewport_binding = match &ready.work {
                session_queue::SessionWork::Scroll { binding, .. }
                | session_queue::SessionWork::ScrollTo { binding, .. }
                | session_queue::SessionWork::SelectionScroll { binding, .. } => Some(binding),
                session_queue::SessionWork::Input { event, .. }
                    if input_returns_viewport_to_bottom(event) =>
                {
                    session_input_binding.as_ref()
                }
                session_queue::SessionWork::UiInput(work)
                    if input_returns_viewport_to_bottom(&work.request.event) =>
                {
                    Some(&work.request.binding)
                }
                _ => None,
            };
            let viewport_refresh_binding = viewport_binding.cloned();
            if let Some(binding) = viewport_binding {
                invalidate_workspace_snapshot_for_scroll(self, binding);
            }
            let executor = self.services.executor.clone();
            let sessions = self.services.sessions.clone();
            let commands = self.services.commands.clone();
            let session_id = ready.session_id.clone();
            let completion_session_id = session_id.clone();
            let ui_input_completion = match &ready.work {
                session_queue::SessionWork::UiInput(work) => Some((
                    work.request.clone(),
                    work.acceptance.clone(),
                    work.command_line_ticket.clone(),
                    work.origin,
                )),
                _ => None,
            };
            // 只在该 session FIFO 真正出队后读取已 ACK 的投影，避免快打时提前绑定旧 input_generation。
            let is_direct_enter = matches!(
                &ready.work,
                session_queue::SessionWork::UiInput(work)
                    if work.acceptance.is_none()
                        && work.origin == session_queue::InputOrigin::Direct
                        && is_plain_command_enter(&work.request.event)
            );
            let direct_enter_context = if is_direct_enter {
                match &ready.work {
                    session_queue::SessionWork::UiInput(work) => self
                        .terminal_command_contexts
                        .get(&work.request.binding.session_id)
                        .filter(|cached| cached.binding == work.request.binding)
                        .map(|cached| cached.context.clone()),
                    _ => None,
                }
            } else {
                None
            };
            let ui_input_submission = match &ready.work {
                session_queue::SessionWork::UiInput(work)
                    if work.acceptance.is_none()
                        && work.origin == session_queue::InputOrigin::Direct =>
                {
                    work.command_line_ticket
                        .as_ref()
                        .and_then(|ticket| {
                            direct_enter_context.as_ref().and_then(|context| {
                                self.command_line_projection
                                    .prepare_submission(ticket, &work.request, context)
                                    .map(|submission| (context.clone(), submission))
                            })
                        })
                        .map(|(before, submission)| {
                            let mut input = work.request.clone();
                            input.expected_context = Some(submission.expected_context);
                            // 应用端仅在 Tracked/plain Enter 完整 ACK 后关联已过滤命令正文。
                            input.projection = UiInputProjection::Tracked;
                            (input, submission.command, before)
                        })
                }
                _ => None,
            };
            if is_direct_enter {
                let ticket_present = matches!(
                    &ready.work,
                    session_queue::SessionWork::UiInput(work)
                        if work.command_line_ticket.is_some()
                );
                let context_available = direct_enter_context.as_ref().is_some_and(|context| {
                    context.availability == TerminalCommandAvailability::Available
                });
                let kitty_keyboard_flags = self
                    .snapshot_for_session(&ready.session_id)
                    .map_or(255_u64, |snapshot| {
                        u64::from(snapshot.modes.kitty_keyboard_flags)
                    });
                tracing::debug!(
                    event = "TERMINAL_TYPED_HISTORY_DIAGNOSTIC",
                    ticket_present = if ticket_present { 1_i64 } else { 0_i64 },
                    context_present = if direct_enter_context.is_some() {
                        1_i64
                    } else {
                        0_i64
                    },
                    context_available = if context_available { 1_i64 } else { 0_i64 },
                    unavailable_reason =
                        terminal_command_unavailable_reason_code(direct_enter_context.as_ref(),),
                    candidate_present = if ui_input_submission.is_some() {
                        1_i64
                    } else {
                        0_i64
                    },
                    kitty_keyboard_flags,
                );
            }
            let command_submitted = ui_input_submission.is_some();
            let resize_size = match &ready.work {
                session_queue::SessionWork::Resize { size, .. } => Some(*size),
                _ => None,
            };
            let history_resize_size = match &ready.work {
                session_queue::SessionWork::HistoryResize { size, .. } => Some(*size),
                _ => None,
            };
            cx.spawn(async move |this, cx| {
                let mut selection_scroll_min_revision = None;
                let (work_result, history_receipt) = match ready.work {
                    session_queue::SessionWork::Input { event, .. } => {
                        let scroll_binding = session_input_binding.clone();
                        (
                            executor
                                .run(move |_| async move {
                                if input_returns_viewport_to_bottom(&event)
                                    && let Some(binding) = scroll_binding
                                {
                                    sessions.scroll_ui_to_offset(binding, 0).await?;
                                }
                                sessions.write_input(session_id.clone(), event).await
                            })
                            .await
                            .map(|()| None),
                            None,
                        )
                    }
                    session_queue::SessionWork::UiInput(work) => {
                        #[cfg(feature = "acceptance-metrics")]
                        let executor_probe_id = echo_probe_id.clone();
                        let session_queue::UiInputWork { request, .. } = *work;
                        if let Some((input, command, before)) = ui_input_submission {
                            let commands_for_write = commands.clone();
                            let should_return_to_bottom =
                                input_returns_viewport_to_bottom(&input.event);
                            let viewport_binding = input.binding.clone();
                            match executor
                                .run(move |cancellation| async move {
                                    if should_return_to_bottom {
                                        sessions
                                            .scroll_ui_to_offset(viewport_binding, 0)
                                            .await?;
                                    }
                                    match commands_for_write {
                                        Some(commands) => {
                                            let submission = commands
                                                .submit_ui_terminal_command(
                                                    input,
                                                    command,
                                                    before,
                                                    true,
                                                    cancellation,
                                                )
                                                .await?;
                                            Ok((submission.context, Some(submission.history)))
                                        }
                                        None => {
                                            let acknowledgement = sessions
                                                .write_ui_command_submission(
                                                    UiTerminalCommandSubmissionRequest {
                                                        input,
                                                        command,
                                                    },
                                                )
                                                .await?;
                                            Ok((acknowledgement, None))
                                        }
                                    }
                                })
                                .await
                            {
                                Ok((context, history)) => (Ok(Some(context)), history),
                                Err(error) => (Err(error), None),
                            }
                        } else {
                            let should_return_to_bottom =
                                input_returns_viewport_to_bottom(&request.event);
                            let viewport_binding = request.binding.clone();
                            let write_result = executor
                                .run(move |_| async move {
                                    if should_return_to_bottom {
                                        sessions
                                            .scroll_ui_to_offset(viewport_binding, 0)
                                            .await?;
                                    }
                                    #[cfg(feature = "acceptance-metrics")]
                                    if let Some(probe_id) = executor_probe_id.as_deref() {
                                        crate::acceptance_metrics::terminal_echo_probe_registry()
                                            .record_phase_for_probe(
                                                probe_id,
                                                crate::acceptance_metrics::TerminalEchoPhase::SessionWriteStarted,
                                                None,
                                            );
                                    }
                                    let result = sessions.write_ui_input(request).await;
                                    #[cfg(feature = "acceptance-metrics")]
                                    if let Some(probe_id) = executor_probe_id.as_deref() {
                                        crate::acceptance_metrics::terminal_echo_probe_registry()
                                            .record_phase_for_probe(
                                                probe_id,
                                                crate::acceptance_metrics::TerminalEchoPhase::SessionWriteReturned,
                                                None,
                                            );
                                    }
                                    result.map(Some)
                                })
                                .await;
                            (write_result, None)
                        }
                    }
                    session_queue::SessionWork::Scroll { binding, lines } => (
                        executor
                            .run(move |_| async move {
                                sessions.scroll_ui(binding, ScrollRequest { lines }).await
                            })
                            .await
                            .map(|()| None),
                        None,
                    ),
                    session_queue::SessionWork::ScrollTo { binding, offset } => (
                        executor
                            .run(move |_| async move {
                                sessions.scroll_ui_to_offset(binding, offset).await
                            })
                            .await
                            .map(|()| None),
                        None,
                    ),
                    session_queue::SessionWork::SelectionScroll {
                        binding,
                        offset,
                        ..
                    } => {
                        let snapshot_binding = binding.clone();
                        match executor
                            .run(move |_| async move {
                                sessions.scroll_ui_to_offset(binding, offset).await?;
                                let snapshot = sessions
                                    .render_snapshot_ui(
                                        snapshot_binding,
                                        RenderSnapshotRequest::default(),
                                    )
                                    .await?;
                                Ok(snapshot.revision)
                            })
                            .await
                        {
                            Ok(revision) => {
                                selection_scroll_min_revision = Some(revision);
                                (Ok(None), None)
                            }
                            Err(error) => (Err(error), None),
                        }
                    }
                    session_queue::SessionWork::Resize { size, .. } => (
                        executor
                            .run(move |_| async move { sessions.resize(session_id, size).await })
                            .await
                            .map(|()| None),
                        None,
                    ),
                    session_queue::SessionWork::HistoryResize { binding, size } => (
                        executor
                            .run(move |_| async move {
                                sessions.resize_ui_view(binding, size).await
                            })
                            .await
                            .map(|()| None),
                        None,
                    ),
                };
                let _ = this.update(cx, |root, cx| {
                    let result = receipt.as_ref().and_then(|receipt| {
                        let current_owner = match receipt.binding_source {
                            SessionCommandBindingSource::InputHandler => {
                                root.session_command_owner(&receipt.session_id)
                            }
                            SessionCommandBindingSource::InputHandlerAndView => {
                                root.session_command_owner(&receipt.session_id)
                            }
                            SessionCommandBindingSource::ReadOnlyView => {
                                root.session_command_view_owner(&receipt.session_id)
                            }
                        };
                        let current_binding = match receipt.binding_source {
                            SessionCommandBindingSource::InputHandler => root
                                .pane_input_handlers
                                .get(&receipt.session_id)
                                .map(|registration| &registration.binding),
                            SessionCommandBindingSource::InputHandlerAndView => root
                                .pane_input_handlers
                                .get(&receipt.session_id)
                                .filter(|registration| {
                                    root.terminal_view_bindings.get(&receipt.session_id)
                                        == Some(&registration.binding)
                                })
                                .map(|registration| &registration.binding),
                            SessionCommandBindingSource::ReadOnlyView => root
                                .terminal_view_bindings
                                .get(&receipt.session_id),
                        };
                        accept_session_command_result(
                            receipt,
                            current_owner,
                            current_binding,
                            work_result,
                        )
                    });
                    let Some(result) = result else {
                        root.session_commands.complete(&completion_session_id);
                        root.start_session_command_pump(cx);
                        cx.notify();
                        return;
                    };
                    if result.is_ok()
                        && let Some(min_snapshot_revision) = selection_scroll_min_revision
                        && let Some(selection_scroll) = receipt
                            .as_ref()
                            .and_then(|receipt| receipt.selection_scroll.as_ref())
                        && root
                            .terminal_view_bindings
                            .get(&selection_scroll.binding.session_id)
                            == Some(&selection_scroll.binding)
                        && root
                            .session_command_view_owner(&selection_scroll.binding.session_id)
                            .as_ref()
                            == Some(&selection_scroll.binding.owner)
                    {
                        let tools = match &selection_scroll.binding.owner {
                            SessionOwner::Ui { tab_id, pane_id } => root
                                .workspace_runtime
                                .pane(pane_id)
                                .filter(|pane| {
                                    pane.tab_id == *tab_id
                                        && pane.session_id.as_ref()
                                            == Some(&selection_scroll.binding.session_id)
                                })
                                .and_then(|_| {
                                    root.workspace_tools
                                        .get(&selection_scroll.binding.session_id)
                                        .cloned()
                                }),
                            SessionOwner::Agent { .. }
                                if root.agent_terminal_session_id.as_ref()
                                    == Some(&selection_scroll.binding.session_id) =>
                            {
                                root.agent_terminal_tools.clone()
                            }
                            SessionOwner::Agent { .. }
                            | SessionOwner::Mcp
                            | SessionOwner::Service { .. } => None,
                        };
                        if let Some(tools) = tools {
                            let binding = selection_scroll.binding.clone();
                            let gesture_id = selection_scroll.gesture_id;
                            let request_id = selection_scroll.request_id;
                            tools.update(cx, |tools, cx| {
                                tools.acknowledge_canvas_selection_scroll(
                                    &binding,
                                    gesture_id,
                                    request_id,
                                    min_snapshot_revision,
                                    cx,
                                );
                            });
                        }
                    }
                    let viewport_operation_failed =
                        result.is_err() && viewport_refresh_binding.is_some();
                    let focus_report = ui_input_completion
                        .as_ref()
                        .is_some_and(|(request, ..)| {
                            matches!(
                                &request.event,
                                kerminal_terminal::input::TerminalInputEvent::Focus(_)
                            )
                        });
                    let ended_panel_observation =
                        (resize_size.is_some() || (result.is_err() && focus_report))
                        .then(|| {
                            receipt.as_ref().and_then(|receipt| {
                                panel_observation_for_receipt(root, receipt)
                            })
                        })
                        .flatten();
                    let ended_resize_is_observed = resize_size.is_some()
                        && receipt
                            .as_ref()
                            .zip(ended_panel_observation.as_ref())
                            .is_some_and(|(receipt, (_, observation))| {
                                receipt.binding.as_ref().is_some_and(|binding| {
                                    observation_proves_session_not_running(
                                        binding,
                                        Some(observation),
                                    )
                                })
                            });
                    let ended_focus_report_is_observed = ui_input_completion
                        .as_ref()
                        .zip(receipt.as_ref())
                        .zip(ended_panel_observation.as_ref())
                        .is_some_and(|(((request, ..), receipt), (target, observation))| {
                            focus_failure_is_proven_ended(
                                request,
                                receipt.binding.as_ref(),
                                target,
                                Some(observation),
                            )
                        });
                    if history_receipt
                        .as_ref()
                        .is_some_and(|history| history.history_error_code.is_some())
                    {
                        root.status = Some(
                            crate::locale::text(
                                "命令已发送到终端，但历史记录未能保存；本次不会重发命令。",
                                "The command was sent, but its history could not be saved. It will not be resent.",
                            )
                            .to_string()
                            .into(),
                        );
                    }
                    let succeeded = match result {
                        Ok(context) => {
                            if let (Some(context), Some((request, acceptance, ticket, _origin))) =
                                (context, ui_input_completion)
                            {
                                root.complete_ui_input_ack(
                                    request,
                                    context,
                                    acceptance,
                                    ticket,
                                    command_submitted,
                                    cx,
                                );
                            }
                            true
                        }
                        Err(error) => {
                            let mut direct_failure = true;
                            if let Some((request, acceptance, ticket, origin)) = ui_input_completion
                            {
                                if let Some(ticket) = ticket.as_ref() {
                                    root.command_line_projection.apply_failure(ticket);
                                }
                                if let Some(write) = acceptance {
                                    root.terminal_command_suggestions.acceptance_input_failed(
                                        &write.acceptance,
                                        write.event_index,
                                    );
                                }
                                if root.is_active_suggestion_binding(&request.binding) {
                                    root.terminal_command_suggestions.clear();
                                    root.suggestion_menu_binding = None;
                                    root.sync_suggestion_menu(cx);
                                }
                                match origin {
                                    session_queue::InputOrigin::Broadcast(generation) => {
                                        direct_failure = false;
                                        root.fail_broadcast_input(
                                            generation,
                                            error.public_message().to_owned(),
                                            cx,
                                        );
                                    }
                                    session_queue::InputOrigin::Direct => {}
                                }
                            }
                            if direct_failure
                                && !ended_resize_is_observed
                                && !ended_focus_report_is_observed
                            {
                                if let Some(SessionOwner::Agent { agent_session_id }) =
                                    receipt.as_ref().map(|receipt| &receipt.owner)
                                {
                                    root.report_agent_terminal_operation_failure(
                                        agent_session_id,
                                        &error,
                                        cx,
                                    );
                                } else {
                                    root.status = Some(if resize_size.is_some()
                                        || history_resize_size.is_some()
                                    {
                                        crate::locale::text(
                                            format!("终端尺寸未同步：{}。请调整窗口大小或重新连接后重试。", error.public_message()),
                                            format!("Terminal resize was not confirmed: {}. Resize the window or reconnect to retry.", error.public_message()),
                                        )
                                    } else {
                                        error.public_message().to_owned().into()
                                    });
                                }
                            }
                            false
                        }
                    };
                    // 先处理 ACK/失败并在需要时将接受计划的下一事件放到同一 lane 队首，再释放当前 in-flight。
                    root.session_commands.complete(&completion_session_id);
                    if !succeeded {
                        cx.notify();
                    }
                    if succeeded && let Some(size) = resize_size {
                        root.failed_session_resizes.remove(&completion_session_id);
                        if let Some(pane_id) = root
                            .workspace_runtime
                            .pane_for_session(&completion_session_id)
                            .cloned()
                        {
                            let committed = root
                                .workspace_runtime
                                .complete_resize(&pane_id, &completion_session_id, size, true)
                                .unwrap_or(false);
                            if committed
                                && let Some(tab_id) = root
                                    .workspace_runtime
                                    .pane(&pane_id)
                                    .map(|pane| pane.tab_id.clone())
                            {
                                root.request_workspace_pane_snapshot(tab_id, pane_id, cx);
                            }
                        } else if root.agent_terminal_session_id.as_ref()
                            == Some(&completion_session_id)
                        {
                            if root.pending_agent_resizes.get(&completion_session_id) == Some(&size)
                            {
                                root.pending_agent_resizes.remove(&completion_session_id);
                            }
                            root.agent_terminal_size = Some(size);
                        }
                    } else if succeeded && let Some(size) = history_resize_size {
                        root.failed_session_resizes.remove(&completion_session_id);
                        if let Some(pane_id) = root
                            .workspace_runtime
                            .pane_for_session(&completion_session_id)
                            .cloned()
                        {
                            let committed = root
                                .workspace_runtime
                                .complete_history_view_resize(
                                    &pane_id,
                                    &completion_session_id,
                                    size,
                                    true,
                                )
                                .unwrap_or(false);
                            if committed
                                && let Some(tab_id) = root
                                    .workspace_runtime
                                    .pane(&pane_id)
                                    .map(|pane| pane.tab_id.clone())
                            {
                                root.request_workspace_pane_snapshot(tab_id, pane_id, cx);
                            }
                        }
                    } else if let Some(size) = resize_size {
                        if ended_resize_is_observed {
                            // ended 的 live resize 失败不锁住后续只读历史重排。
                            root.failed_session_resizes.remove(&completion_session_id);
                        } else {
                            root.failed_session_resizes
                                .insert(completion_session_id.clone(), size);
                        }
                        if let Some(pane_id) = root
                            .workspace_runtime
                            .pane_for_session(&completion_session_id)
                            .cloned()
                        {
                            let _ = root.workspace_runtime.complete_resize(
                                &pane_id,
                                &completion_session_id,
                                size,
                                false,
                            );
                        } else if root.pending_agent_resizes.get(&completion_session_id)
                            == Some(&size)
                        {
                            root.pending_agent_resizes.remove(&completion_session_id);
                        }
                    } else if let Some(size) = history_resize_size {
                        root.failed_session_resizes
                            .insert(completion_session_id.clone(), size);
                        if let Some(pane_id) = root
                            .workspace_runtime
                            .pane_for_session(&completion_session_id)
                            .cloned()
                        {
                            let _ = root.workspace_runtime.complete_history_view_resize(
                                &pane_id,
                                &completion_session_id,
                                size,
                                false,
                            );
                        }
                    }
                    if (resize_size.is_some() || history_resize_size.is_some())
                        && !succeeded
                    {
                        root.refresh_open_terminal_search_for_view_revision(
                            &completion_session_id,
                            cx,
                        );
                    }
                    if viewport_operation_failed
                        && let Some(binding) = viewport_refresh_binding.as_ref()
                        && let SessionOwner::Ui { tab_id, pane_id } = &binding.owner
                        && root
                            .workspace_runtime
                            .snapshot_session_is_current(pane_id, &binding.session_id)
                    {
                        root.request_workspace_pane_snapshot(tab_id.clone(), pane_id.clone(), cx);
                    }
                    root.start_session_command_pump(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// 仅对精确 UI view binding 排队 ended pane 的网格重排，并沿用 session FIFO 保序。
    ///
    /// @author kongweiguang
    pub(super) fn enqueue_history_view_resize(
        &mut self,
        binding: UiSessionBinding,
        size: TerminalSize,
        cx: &mut Context<Self>,
    ) -> bool {
        let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
            return false;
        };
        let pane_is_current = self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
            (pane.phase == RuntimePanePhase::Ready
                || self
                    .workspace_runtime
                    .close_failed_history_binding_matches(pane_id, &binding))
                && pane.tab_id == *tab_id
                && pane.session_id.as_ref() == Some(&binding.session_id)
        });
        if self.terminal_view_bindings.get(&binding.session_id) != Some(&binding)
            || self
                .session_command_view_owner(&binding.session_id)
                .as_ref()
                != Some(&binding.owner)
            || !pane_is_current
            || !allow_canvas_resize_after_failure(
                &mut self.failed_session_resizes,
                &binding.session_id,
                size,
            )
        {
            return false;
        }
        let needs_enqueue = self
            .workspace_runtime
            .history_view_resize_needs_enqueue(pane_id, size)
            .unwrap_or(false);
        if !needs_enqueue {
            return false;
        }
        if self
            .workspace_runtime
            .mark_history_view_resize_pending(pane_id, size)
            .is_err()
        {
            return false;
        }
        let result = self.session_commands.enqueue(
            binding.session_id.clone(),
            session_queue::SessionWork::HistoryResize {
                binding: binding.clone(),
                size,
            },
        );
        if result.is_err() {
            let _ = self.workspace_runtime.complete_history_view_resize(
                pane_id,
                &binding.session_id,
                size,
                false,
            );
            self.failed_session_resizes
                .insert(binding.session_id.clone(), size);
            self.status = Some(
                crate::locale::text(
                    "历史终端尺寸调整未能排队，请调整窗口大小后重试。",
                    "The history view resize could not be queued. Resize the window to retry.",
                )
                .to_string()
                .into(),
            );
            cx.notify();
            return false;
        }
        self.start_session_command_pump(cx);
        true
    }

    /// 仅接受仍由原只读 owner 持有的拖选滚动，并让请求代次随 FIFO 回执原样往返。
    ///
    /// @author kongweiguang
    pub(crate) fn handle_terminal_selection_scroll(
        &mut self,
        binding: UiSessionBinding,
        offset: usize,
        gesture_id: u64,
        request_id: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.terminal_view_bindings.get(&binding.session_id) != Some(&binding)
            || self
                .session_command_view_owner(&binding.session_id)
                .as_ref()
                != Some(&binding.owner)
        {
            return false;
        }
        self.enqueue_session_command(
            SessionCommand::SelectionScroll {
                binding,
                offset,
                gesture_id,
                request_id,
            },
            cx,
        )
    }

    /// 将一次性固定目标输入原子准入现有 FIFO；每个 binding 都须仍是原 pane 的 live UI owner。
    ///
    /// @author kongweiguang
    pub(super) fn enqueue_terminal_input_once(
        &mut self,
        bindings: &[UiSessionBinding],
        event: TerminalInputEvent,
        cx: &mut Context<Self>,
    ) -> Result<usize, session_queue::SessionQueueError> {
        if bindings.is_empty()
            || bindings.iter().any(|binding| {
                let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
                    return true;
                };
                !self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
                    pane.phase == RuntimePanePhase::Ready
                        && pane.tab_id == *tab_id
                        && pane.session_id.as_ref() == Some(&binding.session_id)
                }) || !self
                    .pane_input_handlers
                    .get(&binding.session_id)
                    .is_some_and(|registration| registration.binding == *binding)
            })
        {
            return Err(session_queue::SessionQueueError::SessionUnavailable);
        }
        for binding in bindings {
            self.command_line_projection.invalidate_binding(binding);
        }
        let accepted = self
            .session_commands
            .enqueue_broadcast_once(bindings, event)?;
        self.start_session_command_pump(cx);
        Ok(accepted)
    }

    /// 将输入和控制项放入唯一 session FIFO；准入覆盖来源与票据后移动同一 work Box，正文仅在 ACK 后推进。
    /// 只接收仍由明确 owner 持有的会话，避免已经移除的 pane 使旧焦点或 resize 绕过关闭门禁。
    /// 滚动/SelectionScroll/HistoryResize 只需精确只读 view binding；运行态 resize 要求 live 与 view binding 同代。
    /// 固定绑定的焦点通知不参与命令行编辑票据；其真实 writer ACK 仍在同一 lane 返回。
    /// committed Text 只有经精确已知行 edit ticket 核验后才改为 Tracked，未知输入保留失效语义。
    ///
    /// @author kongweiguang
    pub(super) fn enqueue_session_command(
        &mut self,
        command: SessionCommand,
        cx: &mut Context<Self>,
    ) -> bool {
        let resize_target = match &command {
            SessionCommand::Resize { session_id, size } => Some((session_id.clone(), *size)),
            _ => None,
        };
        let resize_binding = match &command {
            SessionCommand::Resize { session_id, .. } => self
                .pane_input_handlers
                .get(session_id)
                .map(|registration| registration.binding.clone()),
            _ => None,
        };
        let broadcast_input = matches!(
            &command,
            SessionCommand::Input { session_id, event }
                if session_queue::is_broadcastable_input(event)
                    && self.center_panel == CenterPanel::Terminal
                    && self.focused_terminal_session_id().as_ref() == Some(session_id)
                    && self.broadcast_session_ids.is_some()
        );
        let target_ids = if broadcast_input {
            self.broadcast_session_ids
                .as_ref()
                .map(|targets| {
                    targets
                        .iter()
                        .map(|target| target.session_id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            match &command {
                SessionCommand::Input { session_id, .. }
                | SessionCommand::Resize { session_id, .. } => vec![session_id.clone()],
                SessionCommand::Scroll { binding, .. }
                | SessionCommand::ScrollTo { binding, .. }
                | SessionCommand::SelectionScroll { binding, .. } => {
                    vec![binding.session_id.clone()]
                }
                SessionCommand::UiInput(work) => {
                    let request = &work.request;
                    vec![request.binding.session_id.clone()]
                }
            }
        };
        let owner_unavailable = target_ids.iter().any(|session_id| {
            if matches!(
                &command,
                SessionCommand::Scroll { .. }
                    | SessionCommand::ScrollTo { .. }
                    | SessionCommand::SelectionScroll { .. }
            ) {
                self.session_command_view_owner(session_id).is_none()
            } else {
                self.session_command_owner(session_id).is_none()
            }
        });
        let binding_unavailable = match &command {
            SessionCommand::Scroll { binding, .. }
            | SessionCommand::ScrollTo { binding, .. }
            | SessionCommand::SelectionScroll { binding, .. } => {
                self.terminal_view_bindings.get(&binding.session_id) != Some(binding)
            }
            SessionCommand::UiInput(work) => self
                .pane_input_handlers
                .get(&work.request.binding.session_id)
                .is_none_or(|registration| registration.binding != work.request.binding),
            SessionCommand::Input { .. } => target_ids
                .iter()
                .any(|session_id| !self.pane_input_handlers.contains_key(session_id)),
            SessionCommand::Resize { .. } => target_ids.iter().any(|session_id| {
                let Some(binding) = resize_binding.as_ref() else {
                    return true;
                };
                self.pane_input_handlers
                    .get(session_id)
                    .is_none_or(|registration| registration.binding != *binding)
                    || self.terminal_view_bindings.get(session_id) != Some(binding)
            }),
        };
        // 结束会话只留只读视口 binding；所有 shell 写入仍要求活动输入 handler 和 Running owner。
        let unavailable = owner_unavailable || binding_unavailable;
        if broadcast_input {
            if let Some(targets) = self.broadcast_session_ids.as_ref() {
                for target in targets {
                    self.command_line_projection.invalidate_binding(target);
                }
            }
        } else if let SessionCommand::Input { session_id, .. } = &command
            && let Some(registration) = self.pane_input_handlers.get(session_id)
        {
            self.command_line_projection
                .invalidate_binding(&registration.binding);
        }
        let mut command_line_ticket = None;
        let result = if unavailable {
            Err(session_queue::SessionQueueError::SessionUnavailable)
        } else {
            match command {
                SessionCommand::Input { event, .. } if broadcast_input => self
                    .session_commands
                    .enqueue_broadcast_event(self.broadcast_generation, event)
                    .map(|_| ()),
                SessionCommand::Input { session_id, event } => self.session_commands.enqueue(
                    session_id,
                    session_queue::SessionWork::Input {
                        event,
                        origin: session_queue::InputOrigin::Direct,
                    },
                ),
                SessionCommand::UiInput(mut work) => {
                    let ticket = if work.acceptance.is_some() {
                        self.command_line_projection
                            .invalidate_binding(&work.request.binding);
                        None
                    } else if matches!(
                        work.request.event,
                        kerminal_terminal::input::TerminalInputEvent::Focus(_)
                    ) {
                        None
                    } else {
                        self.command_line_projection.register_input(&work.request)
                    };
                    self.command_line_projection
                        .promote_registered_committed_text(&mut work.request, ticket.as_ref());
                    command_line_ticket = ticket.clone();
                    work.command_line_ticket = ticket;
                    // 生产者不能预置来源或票据来授权命令行跟踪写入。
                    work.origin = session_queue::InputOrigin::Direct;
                    let session_id = work.request.binding.session_id.clone();
                    self.session_commands
                        .enqueue(session_id, session_queue::SessionWork::UiInput(work))
                }
                SessionCommand::Scroll { binding, lines } => self.session_commands.enqueue(
                    binding.session_id.clone(),
                    session_queue::SessionWork::Scroll { binding, lines },
                ),
                SessionCommand::ScrollTo { binding, offset } => self.session_commands.enqueue(
                    binding.session_id.clone(),
                    session_queue::SessionWork::ScrollTo { binding, offset },
                ),
                SessionCommand::SelectionScroll {
                    binding,
                    offset,
                    gesture_id,
                    request_id,
                } => self.session_commands.enqueue(
                    binding.session_id.clone(),
                    session_queue::SessionWork::SelectionScroll {
                        binding,
                        offset,
                        gesture_id,
                        request_id,
                    },
                ),
                SessionCommand::Resize { session_id, size } => {
                    let Some(binding) = resize_binding else {
                        return false;
                    };
                    self.session_commands.enqueue(
                        session_id,
                        session_queue::SessionWork::Resize { binding, size },
                    )
                }
            }
        };

        if let Err(error) = result {
            // 未进入 FIFO 的尺寸也不能随 notify 反复准入；这里只抑制重绘，不提交 pending 或伪造 PTY ACK。
            if let Some((session_id, size)) = resize_target.as_ref() {
                self.failed_session_resizes
                    .insert(session_id.clone(), *size);
            }
            if let Some(ticket) = command_line_ticket.as_ref() {
                self.command_line_projection.apply_failure(ticket);
            }
            if broadcast_input && error == session_queue::SessionQueueError::SessionUnavailable {
                self.fail_broadcast_input(
                    self.broadcast_generation,
                    "广播目标正在关闭，广播已停止；请检查目标后重新开始。".to_owned(),
                    cx,
                );
                return false;
            }
            self.status = Some(match error {
                session_queue::SessionQueueError::SessionUnavailable => {
                    "终端正在关闭或等待清理，已拒绝新的输入".into()
                }
                session_queue::SessionQueueError::QueueFull => {
                    if resize_target.is_some() {
                        "终端尺寸暂未同步：输入队列已满。请调整窗口大小后重试。".into()
                    } else {
                        "终端输入队列已满，请稍后重试".into()
                    }
                }
                session_queue::SessionQueueError::BroadcastInactive => {
                    "广播状态已变化，本次输入未发送".into()
                }
                session_queue::SessionQueueError::BroadcastInputNotSupported => {
                    "此终端事件仅作用于当前会话，不会广播".into()
                }
            });
            cx.notify();
            return false;
        }
        if let Some((session_id, size)) = resize_target {
            if let Some(pane_id) = self
                .workspace_runtime
                .pane_for_session(&session_id)
                .cloned()
            {
                let _ = self.workspace_runtime.mark_resize_pending(&pane_id, size);
            } else if self.agent_terminal_session_id.as_ref() == Some(&session_id) {
                self.pending_agent_resizes.insert(session_id, size);
            }
        }
        self.start_session_command_pump(cx);
        true
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_session_command_pump.rs"]
mod tests;
