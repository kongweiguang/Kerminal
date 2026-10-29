//! Agent 命令块预览、精确目标复核与原子提交流程。
//!
//! @author kongweiguang

use std::time::{Duration, Instant};

use crate::features::terminal_tools::{
    CommandBlockEvent, TerminalToolsView, command_blocks::capture_revision_matches,
};
use gpui_kit::{Context, Entity, Window};
use kerminal_application::agents::{
    AgentResumeRecoveryStatus, AgentTextPreview, SendAgentCommandRequest,
};
use kerminal_domain::{
    agents::AgentSessionStatus,
    identity::{AgentSessionId, SessionId},
    sessions::{SessionLifecycle, SessionOwner},
};

use super::super::terminal_tools_runtime::{TerminalToolsOwner, owner_is_current, resolve_owner};
use super::super::{RootView, ToolRailToolId};
use super::agent_pending::{
    AgentSendHistoryContext, PENDING_SEND_TTL, PendingAgentSendEntry, PendingAgentTargetSelection,
};
use super::{RequestOperation, set_notice};

const MAX_AGENT_PREVIEW_BYTES: usize = 32 * 1024;
const PENDING_SEND_POLL_INTERVAL: Duration = Duration::from_millis(32);

/// 发起时捕获的同一命令块来源；revision、会话代次和 owner 必须贯穿预览与确认，不能临时重取。
///
/// @author kongweiguang
pub(super) struct CapturedAgentCommandSource {
    pub(super) source: Entity<TerminalToolsView>,
    pub(super) event: CommandBlockEvent,
    pub(super) source_generation: u64,
    pub(super) capture_revision: u64,
    pub(super) owner: TerminalToolsOwner,
}

/// 同一次已核验的 Agent 输入目标；确认与 writer 准入继续复核这些 ID 和 session generation。
///
/// @author kongweiguang
pub(super) struct CapturedAgentCommandTarget {
    pub(super) agent_id: AgentSessionId,
    pub(super) terminal_session_id: SessionId,
    pub(super) session_generation: u64,
}

/// 冻结命令块来源并打开 Agent 面板；正文只会在确切目标与输入绑定就绪后读取。
///
/// @author kongweiguang
pub(super) fn request_agent_preview(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    event: CommandBlockEvent,
    session_generation: u64,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let Some(revision) = event.capture_revision else {
        return;
    };
    let Some(owner) = resolve_owner(root, &source, &event.session_id, cx) else {
        return;
    };
    if !source_revision_is_current(&source, &event, session_generation, revision, cx) {
        set_notice(
            &source,
            &event,
            session_generation,
            Some(
                crate::locale::text(
                    "命令块已变化，请重新选择发送操作",
                    "The command block changed. Choose the send action again.",
                )
                .to_string(),
            ),
            cx,
        );
        return;
    }
    if root
        .command_block_runtime
        .agent_pending_send
        .active
        .is_some()
    {
        set_notice(
            &source,
            &event,
            session_generation,
            Some(crate::locale::text(
                "已有一条命令等待 Agent 确认，请先完成或取消",
                "A command is already waiting for Agent confirmation. Complete or cancel it first.",
            ).to_string()),
            cx,
        );
        return;
    }
    if root.services.agents.is_none() {
        set_notice(
            &source,
            &event,
            session_generation,
            Some(
                crate::locale::text("Agent 服务不可用", "Agent service is unavailable").to_string(),
            ),
            cx,
        );
        return;
    }

    root.reconcile_agent_panel_for_tab(window, cx);
    ensure_agent_panel_visible(root, window, cx);
    let ticket = root.command_block_runtime.begin(
        owner.clone(),
        RequestOperation::AgentPreview(event.block_id),
    );
    let request_id = root
        .command_block_runtime
        .agent_pending_send
        .next_request_id();
    let source_tab_id = root.agent_panel_tab_id.clone();
    let source_label = crate::locale::text(
        format!("命令块 #{}", event.block_id.ordinal),
        format!("Command block #{}", event.block_id.ordinal),
    )
    .to_string();
    let target_selection = pending_target_selection(root);
    let target_generation = target_selection
        .as_ref()
        .map_or(0, |selection| selection.watch_generation);
    root.command_block_runtime.agent_pending_send.active = Some(PendingAgentSendEntry {
        request: crate::features::agents::AgentPendingSend {
            request_id: request_id.clone(),
            source_label,
        },
        source: CapturedAgentCommandSource {
            source: source.clone(),
            event: event.clone(),
            source_generation: session_generation,
            capture_revision: revision,
            owner,
        },
        source_tab_id,
        ticket: ticket.clone(),
        target_selection,
        target: None,
        preview: None,
        deadline: Instant::now() + PENDING_SEND_TTL,
        target_generation,
        unknown_warning_visible: false,
        submission_in_flight: false,
    });
    sync_pending_send_panel(root, cx);
    set_notice(
        &source,
        &event,
        session_generation,
        Some(
            crate::locale::text(
                "请选择 Agent 会话以生成安全预览",
                "Select an Agent session to create a safe preview.",
            )
            .to_string(),
        ),
        cx,
    );
    start_pending_send_driver(root, request_id, ticket, cx);
}

/// 右栏入口可能处于任意停靠槽；这里只检查投影后是否已打开，再复用用户的标准开关路径。
///
/// @author kongweiguang
fn ensure_agent_panel_visible(
    root: &mut RootView,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let settings = root.current_tool_rail_settings();
    let panels = root.tool_rail_state.project(&settings).panels;
    let already_open = [panels.attached, panels.left, panels.bottom, panels.center]
        .into_iter()
        .any(|tool| tool == Some(ToolRailToolId::AgentLauncher));
    if !already_open {
        root.toggle_tool_rail_panel(ToolRailToolId::AgentLauncher, window, cx);
    }
}

/// 当前面板选择只捕获稳定 Agent/PTY 身份；watch generation 让输入 binding 到达前的等待不被重用。
///
/// @author kongweiguang
fn pending_target_selection(root: &RootView) -> Option<PendingAgentTargetSelection> {
    Some(PendingAgentTargetSelection {
        agent_session_id: root.selected_agent_session_id.clone()?,
        terminal_session_id: root.agent_terminal_session_id.clone()?,
        watch_generation: root.agent_watch_generation,
    })
}

/// 同步当前 Tab 的待发送预览和无正文历史 metadata，Agent 面板实体仍由 Tab 宿主持有。
///
/// @author kongweiguang
fn sync_pending_send_panel(root: &RootView, cx: &mut Context<RootView>) {
    let Some(panel) = root.agents_panel.clone() else {
        return;
    };
    let (pending, preview, history) = root
        .command_block_runtime
        .agent_pending_send
        .panel_state(root.agent_panel_tab_id.as_ref());
    panel.update(cx, |panel, cx| {
        panel.set_pending_send_state(pending, preview, history, cx);
    });
}

/// 用户选择 Agent 后固定请求目标；未运行的会话仍可被替换选择，但不会被自动启动或恢复。
///
/// @author kongweiguang
pub(in crate::root) fn select_pending_send_target(
    root: &mut RootView,
    agent_session_id: Option<&AgentSessionId>,
    terminal_session_id: Option<&SessionId>,
    cx: &mut Context<RootView>,
) {
    let Some(agent_session_id) = agent_session_id.cloned() else {
        return;
    };
    let Some(terminal_session_id) = terminal_session_id.cloned() else {
        return;
    };
    let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_mut()
    else {
        return;
    };
    if entry.source_tab_id != root.agent_panel_tab_id || entry.target_selection.is_some() {
        return;
    }
    entry.target_generation = root.agent_watch_generation;
    entry.target_selection = Some(PendingAgentTargetSelection {
        agent_session_id,
        terminal_session_id,
        watch_generation: root.agent_watch_generation,
    });
    sync_pending_send_panel(root, cx);
    cx.notify();
}

/// 切回来源 Tab 时重放面板投影并允许后台等候的同一请求继续核对原目标。
///
/// @author kongweiguang
pub(in crate::root) fn active_agent_panel_changed(root: &mut RootView, cx: &mut Context<RootView>) {
    refresh_pending_target_watch_generation(root);
    sync_pending_send_panel(root, cx);
    let Some(panel) = root.agents_panel.clone() else {
        return;
    };
    let (agent, terminal) = {
        let panel = panel.read(cx);
        (
            panel.selected_agent_session_id(),
            panel.selected_terminal_session_id(),
        )
    };
    select_pending_send_target(root, agent.as_ref(), terminal.as_ref(), cx);
}

/// A reselected identical Agent/PTY may use a new watcher, but never a new terminal session generation.
///
/// @author kongweiguang
fn refresh_pending_target_watch_generation(root: &mut RootView) {
    let source_tab_id = root.agent_panel_tab_id.clone();
    let agent_id = root.selected_agent_session_id.clone();
    let terminal_session_id = root.agent_terminal_session_id.clone();
    let watch_generation = root.agent_watch_generation;
    let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_mut()
    else {
        return;
    };
    if entry.source_tab_id != source_tab_id || entry.submission_in_flight {
        return;
    }
    let Some(target) = entry.target.as_ref() else {
        return;
    };
    if agent_id.as_ref() != Some(&target.agent_id)
        || terminal_session_id.as_ref() != Some(&target.terminal_session_id)
    {
        return;
    }
    let binding_matches = root
        .pane_input_handlers
        .get(&target.terminal_session_id)
        .is_some_and(|binding| {
            binding.binding.session_generation == target.session_generation
                && binding.binding.owner
                    == (SessionOwner::Agent {
                        agent_session_id: target.agent_id.clone(),
                    })
        });
    if binding_matches {
        entry.target_generation = watch_generation;
        if let Some(selection) = entry.target_selection.as_mut() {
            selection.watch_generation = watch_generation;
        }
    }
}

struct PendingSendReady {
    source: CapturedAgentCommandSource,
    target: CapturedAgentCommandTarget,
    text: String,
    previous_submission_unknown: bool,
}

struct PendingSendContext {
    source: CapturedAgentCommandSource,
    target: CapturedAgentCommandTarget,
    previous_submission_unknown: bool,
}

enum PendingSendPreparation {
    Wait,
    Ready(Box<PendingSendReady>),
    Expired,
    Failed(String),
    Stale,
}

/// Poll only the short UI binding handoff; the bounded deadline prevents an Agent selection from waiting forever.
///
/// @author kongweiguang
fn start_pending_send_driver(
    root: &mut RootView,
    request_id: String,
    ticket: super::RequestTicket,
    cx: &mut Context<RootView>,
) {
    let Some(agents) = root.services.agents.clone() else {
        return;
    };
    let executor = root.services.executor.clone();
    let callback_ticket = ticket.clone();
    let task_cancellation = ticket.cancellation.clone();
    let task = cx.spawn(async move |this, cx| {
        loop {
            let prepared = this.update(cx, |root, cx| {
                prepare_pending_send(root, &request_id, &callback_ticket, cx)
            });
            match prepared {
                Ok(PendingSendPreparation::Wait) => {
                    cx.background_executor()
                        .timer(PENDING_SEND_POLL_INTERVAL)
                        .await;
                }
                Ok(PendingSendPreparation::Expired) => {
                    let _ = this.update(cx, |root, cx| {
                        finish_pending_send_request(
                            root,
                            &request_id,
                            crate::features::agents::AgentSendOutcome::Expired,
                            cx,
                        );
                    });
                    return;
                }
                Ok(PendingSendPreparation::Stale) => {
                    let _ = this.update(cx, |root, cx| {
                        finish_pending_send_request(
                            root,
                            &request_id,
                            crate::features::agents::AgentSendOutcome::Failed,
                            cx,
                        );
                    });
                    return;
                }
                Ok(PendingSendPreparation::Failed(message)) => {
                    let _ = this.update(cx, |root, cx| {
                        fail_pending_send(root, &request_id, message, cx);
                    });
                    return;
                }
                Ok(PendingSendPreparation::Ready(ready)) => {
                    let PendingSendReady {
                        source,
                        target,
                        text,
                        previous_submission_unknown,
                    } = *ready;
                    let agent_id = target.agent_id.clone();
                    let context = PendingSendContext {
                        source,
                        target,
                        previous_submission_unknown,
                    };
                    let preview_agents = agents.clone();
                    let preview_cancellation = task_cancellation.clone();
                    let result = executor
                        .run_cancellable(
                            preview_cancellation,
                            move |operation_cancellation| async move {
                                let preview = preview_agents
                                    .preview_text(
                                        agent_id.clone(),
                                        text,
                                        MAX_AGENT_PREVIEW_BYTES,
                                        operation_cancellation.clone(),
                                    )
                                    .await?;
                                let view =
                                    preview_agents.get(agent_id, operation_cancellation).await?;
                                Ok((preview, view, context))
                            },
                        )
                        .await;
                    let completion_request_id = request_id.clone();
                    let completion_ticket = callback_ticket.clone();
                    let _ = this.update(cx, move |root, cx| {
                        complete_pending_send_preview(
                            root,
                            completion_request_id,
                            completion_ticket,
                            result,
                            cx,
                        );
                    });
                }
                Err(_) => return,
            }
        }
    });
    root.command_block_runtime.install_task(&ticket, task);
}

/// 将当前状态归约为等待、失败或一次精确的 preview 请求，期间不会缓存原始命令正文。
///
/// @author kongweiguang
fn prepare_pending_send(
    root: &mut RootView,
    request_id: &str,
    ticket: &super::RequestTicket,
    cx: &mut Context<RootView>,
) -> PendingSendPreparation {
    if !root.command_block_runtime.is_current(ticket) {
        return PendingSendPreparation::Stale;
    }
    let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_ref()
        .filter(|entry| entry.request.request_id == request_id)
    else {
        return PendingSendPreparation::Stale;
    };
    if entry.submission_in_flight || entry.preview.is_some() {
        return if !entry.submission_in_flight && Instant::now() >= entry.deadline {
            PendingSendPreparation::Expired
        } else {
            PendingSendPreparation::Wait
        };
    }
    if Instant::now() >= entry.deadline {
        return PendingSendPreparation::Expired;
    }
    let Some(selection) = entry.target_selection.clone() else {
        return PendingSendPreparation::Wait;
    };
    if root.agent_panel_tab_id != entry.source_tab_id {
        return PendingSendPreparation::Wait;
    }
    if root.selected_agent_session_id.as_ref() != Some(&selection.agent_session_id)
        || root.agent_terminal_session_id.as_ref() != Some(&selection.terminal_session_id)
    {
        return PendingSendPreparation::Failed(
            crate::locale::text(
                "Agent 目标已变化；未发送，请重新选择命令块",
                "The Agent target changed. Nothing was sent; select the command block again.",
            )
            .to_string(),
        );
    }
    if root.agent_watch_generation != selection.watch_generation {
        if let Some(entry) = root
            .command_block_runtime
            .agent_pending_send
            .active
            .as_mut()
            && entry.request.request_id == request_id
        {
            if let Some(selection) = entry.target_selection.as_mut() {
                selection.watch_generation = root.agent_watch_generation;
            }
            entry.target_generation = root.agent_watch_generation;
            entry.target = None;
        }
        return PendingSendPreparation::Wait;
    }
    if root
        .agent_terminal_lifecycle
        .is_some_and(|lifecycle| lifecycle != SessionLifecycle::Running)
    {
        return PendingSendPreparation::Failed(
            crate::locale::text(
                "目标 Agent 终端未运行，未发送内容",
                "The selected Agent terminal is not running. Nothing was sent.",
            )
            .to_string(),
        );
    }
    let Some(binding) = root.pane_input_handlers.get(&selection.terminal_session_id) else {
        if root
            .pane_input_binding_failures
            .contains(&selection.terminal_session_id)
        {
            return PendingSendPreparation::Failed(
                crate::locale::text(
                    "Agent 终端输入绑定失败，未发送内容",
                    "The Agent terminal input binding failed. Nothing was sent.",
                )
                .to_string(),
            );
        }
        return PendingSendPreparation::Wait;
    };
    if binding.binding.session_id != selection.terminal_session_id
        || binding.binding.owner
            != (SessionOwner::Agent {
                agent_session_id: selection.agent_session_id.clone(),
            })
    {
        return PendingSendPreparation::Failed(
            crate::locale::text(
                "Agent 终端输入绑定已变化，未发送内容",
                "The Agent terminal input binding changed. Nothing was sent.",
            )
            .to_string(),
        );
    }

    let source = CapturedAgentCommandSource {
        source: entry.source.source.clone(),
        event: entry.source.event.clone(),
        source_generation: entry.source.source_generation,
        capture_revision: entry.source.capture_revision,
        owner: entry.source.owner.clone(),
    };
    if !owner_is_current(root, &source.owner, cx)
        || resolve_owner(root, &source.source, &source.event.session_id, cx)
            .is_none_or(|owner| owner != source.owner)
        || !source_revision_is_current(
            &source.source,
            &source.event,
            source.source_generation,
            source.capture_revision,
            cx,
        )
    {
        return PendingSendPreparation::Failed(crate::locale::text(
            "原命令块或终端目标已失效，未读取正文",
            "The original command block or terminal target is no longer valid. Its text was not read.",
        ).to_string());
    }
    let Some(text) = source
        .source
        .read(cx)
        .command_block_export_text(source.event.block_id, cx)
    else {
        return PendingSendPreparation::Failed(
            crate::locale::text(
                "命令块正文不可用，未发送内容",
                "The command block text is unavailable. Nothing was sent.",
            )
            .to_string(),
        );
    };
    let previous_submission_unknown = root.command_block_runtime.agent_submission_is_uncertain(
        &source.owner.session_id,
        source.event.block_id,
        &selection.agent_session_id,
    );
    PendingSendPreparation::Ready(Box::new(PendingSendReady {
        source,
        target: CapturedAgentCommandTarget {
            agent_id: selection.agent_session_id,
            terminal_session_id: selection.terminal_session_id,
            session_generation: binding.binding.session_generation,
        },
        text,
        previous_submission_unknown,
    }))
}

/// Agent service 回执仍须匹配原请求、来源 revision 和 terminal owner，迟到结果不能落到新目标。
///
/// @author kongweiguang
fn complete_pending_send_preview(
    root: &mut RootView,
    request_id: String,
    ticket: super::RequestTicket,
    result: Result<
        (
            AgentTextPreview,
            kerminal_application::agents::AgentSessionView,
            PendingSendContext,
        ),
        kerminal_application::AppError,
    >,
    cx: &mut Context<RootView>,
) {
    if !root.command_block_runtime.is_current(&ticket)
        || root
            .command_block_runtime
            .agent_pending_send
            .active
            .as_ref()
            .is_none_or(|entry| entry.request.request_id != request_id)
    {
        return;
    }
    let Some(deadline) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_ref()
        .filter(|entry| entry.request.request_id == request_id)
        .map(|entry| entry.deadline)
    else {
        return;
    };
    if Instant::now() >= deadline {
        finish_pending_send_request(
            root,
            &request_id,
            crate::features::agents::AgentSendOutcome::Expired,
            cx,
        );
        return;
    }
    let (preview, view, context) = match result {
        Ok(result) => result,
        Err(error) => {
            fail_pending_send(root, &request_id, error.public_message().to_owned(), cx);
            return;
        }
    };
    if !owner_is_current(root, &context.source.owner, cx)
        || resolve_owner(
            root,
            &context.source.source,
            &context.source.event.session_id,
            cx,
        )
        .is_none_or(|owner| owner != context.source.owner)
        || !source_revision_is_current(
            &context.source.source,
            &context.source.event,
            context.source.source_generation,
            context.source.capture_revision,
            cx,
        )
        || !agent_view_matches_target(&view, &context.target)
    {
        fail_pending_send(
            root,
            &request_id,
            crate::locale::text(
                "来源命令块或 Agent 目标已变化，预览已丢弃",
                "The source command block or Agent target changed. The preview was discarded.",
            )
            .to_string(),
            cx,
        );
        return;
    }
    if let Some(recovery) = view.resume_recovery.as_ref() {
        let message = match recovery.status {
            AgentResumeRecoveryStatus::Pending => crate::locale::text(
                "指定 Agent 的恢复提交尚未核验；未打开预览，也未发送内容。请先完成恢复核验。",
                "The selected Agent has an unverified resume commit. No preview was opened and nothing was sent.",
            ),
            AgentResumeRecoveryStatus::Unknown => crate::locale::text(
                "指定 Agent 的恢复结果未知；未打开预览，也未发送内容。请先检查该 Agent 终端并完成恢复核验。",
                "The selected Agent's resume outcome is unknown. Inspect its terminal and resolve recovery first.",
            ),
        }
        .to_string();
        fail_pending_send(root, &request_id, message, cx);
        return;
    }
    let Some(terminal) = view.terminal.as_ref().filter(|terminal| {
        terminal.session_id == context.target.terminal_session_id
            && terminal.lifecycle == SessionLifecycle::Running
            && terminal.owner
                == (SessionOwner::Agent {
                    agent_session_id: context.target.agent_id.clone(),
                })
    }) else {
        fail_pending_send(
            root,
            &request_id,
            crate::locale::text(
                "目标 Agent 已不再活动，未发送内容",
                "The selected Agent is no longer active. Nothing was sent.",
            )
            .to_string(),
            cx,
        );
        return;
    };
    let (source_tab_id, target_generation) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_ref()
        .filter(|entry| entry.request.request_id == request_id)
        .map(|entry| (entry.source_tab_id.clone(), entry.target_generation))
        .unwrap_or_default();
    if root.agent_panel_tab_id == source_tab_id
        && (root.selected_agent_session_id.as_ref() != Some(&context.target.agent_id)
            || root.agent_terminal_session_id.as_ref() != Some(&terminal.session_id)
            || root.agent_watch_generation != target_generation
            || root
                .pane_input_handlers
                .get(&terminal.session_id)
                .is_none_or(|binding| {
                    binding.binding.session_generation != context.target.session_generation
                        || binding.binding.owner
                            != (SessionOwner::Agent {
                                agent_session_id: context.target.agent_id.clone(),
                            })
                }))
    {
        fail_pending_send(
            root,
            &request_id,
            crate::locale::text(
                "Agent 终端输入绑定已变化，预览已丢弃",
                "The Agent terminal input binding changed. The preview was discarded.",
            )
            .to_string(),
            cx,
        );
        return;
    }

    let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_mut()
    else {
        return;
    };
    if entry.request.request_id != request_id {
        return;
    }
    let preview_id = uuid::Uuid::new_v4().to_string();
    let deadline = Instant::now() + PENDING_SEND_TTL;
    let expires_at = (chrono::Utc::now()
        + chrono::Duration::seconds(PENDING_SEND_TTL.as_secs() as i64))
    .to_rfc3339();
    let preview = crate::features::agents::AgentPendingSendPreview {
        agent_session_id: context.target.agent_id.clone(),
        byte_length: preview.text.len(),
        expires_at,
        preview_id,
        redacted: preview.redacted,
        request_id,
        text: preview.text,
        truncated: preview.truncated,
    };
    entry.unknown_warning_visible = context.previous_submission_unknown;
    entry.target = Some(context.target);
    entry.preview = Some(preview);
    entry.deadline = deadline;
    sync_pending_send_panel(root, cx);
    cx.notify();
}

/// 校验 Agent service 投影为同一活动 CLI，恢复状态由调用方单独处理。
///
/// @author kongweiguang
fn agent_view_matches_target(
    view: &kerminal_application::agents::AgentSessionView,
    target: &CapturedAgentCommandTarget,
) -> bool {
    view.session.agent_session_id == target.agent_id
        && view.session.status == AgentSessionStatus::Active
        && view.terminal.as_ref().is_some_and(|terminal| {
            terminal.session_id == target.terminal_session_id
                && terminal.lifecycle == SessionLifecycle::Running
                && terminal.owner
                    == (SessionOwner::Agent {
                        agent_session_id: target.agent_id.clone(),
                    })
        })
}

/// 在任何异步结果前移前撤销 ticket 并投影终态；unknown 由根级风险记录单独保留。
///
/// @author kongweiguang
fn finish_pending_send_request(
    root: &mut RootView,
    request_id: &str,
    outcome: crate::features::agents::AgentSendOutcome,
    cx: &mut Context<RootView>,
) {
    let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .take(request_id)
    else {
        return;
    };
    entry.ticket.cancellation.cancel();
    root.command_block_runtime.finish(&entry.ticket);
    root.command_block_runtime
        .agent_pending_send
        .entry_outcome(&entry, outcome);
    sync_pending_send_panel(root, cx);
    cx.notify();
}

/// 失败只报告给冻结的来源命令块，并按 Agent 会话 metadata 保存已知失败。
///
/// @author kongweiguang
fn fail_pending_send(
    root: &mut RootView,
    request_id: &str,
    message: String,
    cx: &mut Context<RootView>,
) {
    if let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_ref()
        && entry.request.request_id == request_id
    {
        if source_block_is_current(
            &entry.source.source,
            &entry.source.event,
            entry.source.source_generation,
            cx,
        ) {
            set_notice(
                &entry.source.source,
                &entry.source.event,
                entry.source.source_generation,
                Some(message.clone()),
                cx,
            );
        }
        root.status = Some(message.into());
    }
    finish_pending_send_request(
        root,
        request_id,
        crate::features::agents::AgentSendOutcome::Failed,
        cx,
    );
}

/// 用户取消只撤销尚未提交的预览；已进入 writer 的操作不再允许由面板改写其结果。
///
/// @author kongweiguang
pub(in crate::root) fn cancel_pending_send(
    root: &mut RootView,
    request_id: &str,
    cx: &mut Context<RootView>,
) {
    if root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_ref()
        .is_none_or(|entry| entry.request.request_id != request_id || entry.submission_in_flight)
    {
        return;
    }
    finish_pending_send_request(
        root,
        request_id,
        crate::features::agents::AgentSendOutcome::Cancelled,
        cx,
    );
}

/// 面板仅提交 request、preview 和 Agent 身份；Root 再验证 TTL、捕获目标和 writer binding。
///
/// @author kongweiguang
pub(in crate::root) fn confirm_pending_send(
    root: &mut RootView,
    agent_session_id: AgentSessionId,
    request_id: &str,
    preview_id: &str,
    cx: &mut Context<RootView>,
) {
    let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_ref()
    else {
        return;
    };
    if entry.request.request_id != request_id
        || entry.submission_in_flight
        || entry.preview.as_ref().is_none_or(|preview| {
            preview.request_id != request_id
                || preview.preview_id != preview_id
                || preview.agent_session_id != agent_session_id
                || Instant::now() >= entry.deadline
        })
    {
        if entry.request.request_id == request_id && Instant::now() >= entry.deadline {
            finish_pending_send_request(
                root,
                request_id,
                crate::features::agents::AgentSendOutcome::Expired,
                cx,
            );
        }
        return;
    }
    let Some(target) = entry.target.as_ref().filter(|target| {
        target.agent_id == agent_session_id
            && entry.target_selection.as_ref().is_some_and(|selection| {
                selection.agent_session_id == target.agent_id
                    && selection.terminal_session_id == target.terminal_session_id
            })
    }) else {
        fail_pending_send(
            root,
            request_id,
            crate::locale::text(
                "预览目标已失效，未发送内容",
                "The preview target is no longer valid. Nothing was sent.",
            )
            .to_string(),
            cx,
        );
        return;
    };
    if root.agent_panel_tab_id != entry.source_tab_id {
        return;
    }
    if root.agent_watch_generation != entry.target_generation {
        fail_pending_send(
            root,
            request_id,
            crate::locale::text(
                "Agent 终端目标已重新绑定，预览已失效",
                "The Agent terminal target was rebound. The preview is no longer valid.",
            )
            .to_string(),
            cx,
        );
        return;
    }
    let preview = entry.preview.as_ref().expect("preview checked above");
    let history = AgentSendHistoryContext {
        agent_session_id: agent_session_id.clone(),
        byte_length: preview.byte_length,
        created_at: chrono::Utc::now().to_rfc3339(),
        request_id: request_id.to_owned(),
        source_label: entry.request.source_label.clone(),
        tab_id: entry.source_tab_id.clone(),
    };
    let source = CapturedAgentCommandSource {
        source: entry.source.source.clone(),
        event: entry.source.event.clone(),
        source_generation: entry.source.source_generation,
        capture_revision: entry.source.capture_revision,
        owner: entry.source.owner.clone(),
    };
    let target = CapturedAgentCommandTarget {
        agent_id: target.agent_id.clone(),
        terminal_session_id: target.terminal_session_id.clone(),
        session_generation: target.session_generation,
    };
    let preview = AgentTextPreview {
        text: preview.text.clone(),
        redacted: preview.redacted,
        truncated: preview.truncated,
    };
    let unknown_warning_acknowledged = entry.unknown_warning_visible;
    if let Some(entry) = root
        .command_block_runtime
        .agent_pending_send
        .active
        .as_mut()
    {
        entry.submission_in_flight = true;
    }
    let started = begin_agent_submission(
        root,
        AgentSubmissionRequest {
            source_context: source,
            target_context: target,
            preview,
            unknown_warning_acknowledged,
            request_id: request_id.to_owned(),
            history_context: history.clone(),
        },
        cx,
    );
    if !started {
        finish_pending_send_request(
            root,
            request_id,
            crate::features::agents::AgentSendOutcome::Failed,
            cx,
        );
    } else {
        if let Some(entry) = root
            .command_block_runtime
            .agent_pending_send
            .active
            .as_mut()
            && entry.request.request_id == request_id
        {
            entry.preview = None;
        }
        sync_pending_send_panel(root, cx);
    }
}

/// 确认时冻结的所有输入一起传递，避免调用方混合不同预览的目标、未知结果确认和历史上下文。
struct AgentSubmissionRequest {
    source_context: CapturedAgentCommandSource,
    target_context: CapturedAgentCommandTarget,
    preview: AgentTextPreview,
    unknown_warning_acknowledged: bool,
    request_id: String,
    history_context: AgentSendHistoryContext,
}

/// 在确认回调中同时复核冻结请求的 source revision、活动 Agent、输入 owner 和 session generation。
///
/// @author kongweiguang
fn begin_agent_submission(
    root: &mut RootView,
    request: AgentSubmissionRequest,
    cx: &mut Context<RootView>,
) -> bool {
    let AgentSubmissionRequest {
        source_context,
        target_context,
        preview,
        unknown_warning_acknowledged,
        request_id,
        history_context,
    } = request;
    let CapturedAgentCommandSource {
        source,
        event,
        source_generation,
        capture_revision,
        owner: expected_owner,
    } = source_context;
    let CapturedAgentCommandTarget {
        agent_id,
        terminal_session_id: agent_terminal_session_id,
        session_generation: agent_terminal_generation,
    } = target_context;
    let Some(current_owner) = resolve_owner(root, &source, &event.session_id, cx) else {
        return false;
    };
    if current_owner != expected_owner
        || !owner_is_current(root, &expected_owner, cx)
        || root.selected_agent_session_id.as_ref() != Some(&agent_id)
        || root.agent_terminal_session_id.as_ref() != Some(&agent_terminal_session_id)
        || !source_revision_is_current(&source, &event, source_generation, capture_revision, cx)
    {
        set_notice(
            &source,
            &event,
            source_generation,
            Some(
                crate::locale::text(
                    "来源或目标已变化，未发送；请重新打开预览",
                    "The source or target changed; nothing was sent. Open the preview again",
                )
                .to_string(),
            ),
            cx,
        );
        return false;
    }

    let prior_submission_unknown = root.command_block_runtime.agent_submission_is_uncertain(
        &expected_owner.session_id,
        event.block_id,
        &agent_id,
    );
    if prior_submission_unknown && !unknown_warning_acknowledged {
        if source_block_is_current(&source, &event, source_generation, cx) {
            source.update(cx, |tools, cx| {
                tools.mark_agent_submission_unknown(
                    &event.session_id,
                    source_generation,
                    event.block_id,
                    agent_id.clone(),
                    cx,
                );
            });
        }
        set_notice(
            &source,
            &event,
            source_generation,
            Some(crate::locale::text(
                "结果刚变为未知；未发送。请重新打开精确预览，检查指定 Agent 终端后再确认。",
                "The previous result became unknown after this preview; nothing was sent. Reopen the exact preview, inspect the selected Agent terminal, then confirm explicitly.",
            ).to_string()),
            cx,
        );
        return false;
    }

    let Some(binding) = root.pane_input_handlers.get(&agent_terminal_session_id) else {
        set_notice(
            &source,
            &event,
            source_generation,
            Some(
                crate::locale::text(
                    "Agent 终端输入绑定已失效，未发送",
                    "The Agent terminal input binding expired; nothing was sent",
                )
                .to_string(),
            ),
            cx,
        );
        return false;
    };
    if binding.binding.session_id != agent_terminal_session_id
        || binding.binding.session_generation != agent_terminal_generation
        || binding.binding.owner
            != (SessionOwner::Agent {
                agent_session_id: agent_id.clone(),
            })
    {
        set_notice(
            &source,
            &event,
            source_generation,
            Some(
                crate::locale::text(
                    "Agent 终端代次已变化，未发送",
                    "The Agent terminal generation changed; nothing was sent",
                )
                .to_string(),
            ),
            cx,
        );
        return false;
    }

    let Some(agents) = root.services.agents.clone() else {
        set_notice(
            &source,
            &event,
            source_generation,
            Some(
                crate::locale::text("Agent 服务不可用", "Agent service is unavailable").to_string(),
            ),
            cx,
        );
        return false;
    };
    let Some(ticket) = root.command_block_runtime.begin_exclusive(
        expected_owner.clone(),
        RequestOperation::AgentSend(event.block_id),
    ) else {
        set_notice(
            &source,
            &event,
            source_generation,
            Some(
                crate::locale::text(
                    "此命令块正在提交，请等待当前结果",
                    "This command block is already being submitted; wait for its result",
                )
                .to_string(),
            ),
            cx,
        );
        return false;
    };
    let request = SendAgentCommandRequest {
        agent_session_id: agent_id.clone(),
        terminal_session_id: agent_terminal_session_id.clone(),
        expected_session_generation: agent_terminal_generation,
        text: preview.text,
    };
    let executor = root.services.executor.clone();
    let callback_ticket = ticket.clone();
    let callback_owner = expected_owner;
    let callback_agent_id = agent_id.clone();
    let callback_source = source.clone();
    let callback_event = event.clone();
    let callback_request_id = request_id;
    let callback_history_context = history_context;
    let task_cancellation = ticket.cancellation.clone();
    source.update(cx, |tools, cx| {
        let pending_message = if prior_submission_unknown {
            crate::locale::text(
                format!(
                    "上次向 Agent {agent_id} 的提交结果仍未知，可能重复；正在提交本次已确认文本。"
                ),
                format!(
                    "The previous submission to Agent {agent_id} is still unknown and may duplicate; submitting only this newly confirmed text."
                ),
            )
        } else {
            crate::locale::text(
                "正在提交到指定 Agent…",
                "Submitting to the selected Agent…",
            )
        };
        tools.set_command_block_notice(
            &event.session_id,
            source_generation,
            event.block_id,
            Some(pending_message.to_string()),
            cx,
        );
    });

    let task = cx.spawn(async move |this, cx| {
        let result = executor
            .run_cancellable(task_cancellation, move |operation_cancellation| async move {
                agents
                    .send_command_block_text(request, operation_cancellation)
                    .await
            })
            .await;
        let _ = this.update(cx, |root, cx| {
            if !root.command_block_runtime.is_current(&callback_ticket) {
                return;
            }
            let uncertain = matches!(
                &result,
                Err(error)
                    if error.public_code()
                        == kerminal_application::PublicErrorCode::OutcomeUnknown
            );
            let history_outcome = if uncertain {
                None
            } else if result.is_ok() {
                Some(crate::features::agents::AgentSendOutcome::Sent)
            } else {
                Some(crate::features::agents::AgentSendOutcome::Failed)
            };
            if uncertain {
                root.command_block_runtime.mark_agent_submission_uncertain(
                    &callback_owner.session_id,
                    callback_event.block_id,
                    callback_agent_id.clone(),
                );
                if source_block_is_current(
                    &callback_source,
                    &callback_event,
                    source_generation,
                    cx,
                ) {
                    callback_source.update(cx, |tools, cx| {
                        tools.mark_agent_submission_unknown(
                            &callback_event.session_id,
                            source_generation,
                            callback_event.block_id,
                            callback_agent_id.clone(),
                            cx,
                        );
                    });
                }
            }
            root.command_block_runtime.finish(&callback_ticket);
            if let Some(entry) = root
                .command_block_runtime
                .agent_pending_send
                .take(&callback_request_id)
            {
                entry.ticket.cancellation.cancel();
                root.command_block_runtime.finish(&entry.ticket);
            }
            if let Some(outcome) = history_outcome {
                root.command_block_runtime
                    .agent_pending_send
                    .record_outcome(callback_history_context.clone(), outcome);
            }
            sync_pending_send_panel(root, cx);

            let source_is_current = source_revision_is_current(
                &callback_source,
                &callback_event,
                source_generation,
                capture_revision,
                cx,
            );
            let source_owner_is_current = owner_is_current(root, &callback_owner, cx);

            let message = match result {
                Ok(receipt) => crate::locale::text(
                    format!(
                        "输入 writer 已确认 {} 字节；这不表示 Agent CLI 已执行",
                        receipt.acknowledged_bytes
                    ),
                    format!(
                        "Input writer acknowledged {} bytes; this does not mean the Agent CLI executed it",
                        receipt.acknowledged_bytes
                    ),
                )
                .to_string(),
                Err(error)
                    if error.public_code()
                        == kerminal_application::PublicErrorCode::OutcomeUnknown =>
                {
                    crate::locale::text(
                        "提交结果未知；未自动重试，请先检查指定 Agent 终端",
                        "Submission outcome is unknown; no retry was attempted. Inspect the selected Agent terminal first",
                    )
                    .to_string()
                }
                Err(error) => error.public_message().to_owned(),
            };
            let message = if prior_submission_unknown {
                crate::locale::text(
                    format!(
                        "此前向 Agent {callback_agent_id} 的提交结果仍未知，可能重复。本次操作：{message}"
                    ),
                    format!(
                        "The earlier submission to Agent {callback_agent_id} remains unknown and may duplicate. This attempt: {message}"
                    ),
                )
                .to_string()
            } else {
                message
            };
            if source_is_current {
                callback_source.update(cx, |tools, cx| {
                    if source_binding_matches(
                        tools,
                        &callback_event,
                        source_generation,
                        capture_revision,
                        cx,
                    ) {
                        tools.set_command_block_notice(
                            &callback_event.session_id,
                            source_generation,
                            callback_event.block_id,
                            Some(message.clone()),
                            cx,
                        );
                    }
                });
            }
            if source_owner_is_current {
                root.status = Some(message.into());
                cx.notify();
            }
        });
    });
    root.command_block_runtime.install_task(&ticket, task);
    true
}

/// 查询回执与确认框都必须仍指向发起时的 session、block 和 transcript revision。
fn source_revision_is_current(
    source: &Entity<TerminalToolsView>,
    event: &CommandBlockEvent,
    source_generation: u64,
    capture_revision: u64,
    cx: &gpui_kit::App,
) -> bool {
    source
        .read(cx)
        .command_block_binding(cx)
        .is_some_and(|(session, generation)| {
            session == event.session_id
                && generation == source_generation
                && source.read(cx).contains_command_block(event.block_id, cx)
                && capture_revision_matches(
                    Some(capture_revision),
                    source
                        .read(cx)
                        .command_block_export_revision(event.block_id, cx),
                )
        })
}

/// 在 Entity update 闭包内检查同一 binding 与 transcript revision，避免持有陈旧卡片。
fn source_binding_matches(
    tools: &TerminalToolsView,
    event: &CommandBlockEvent,
    source_generation: u64,
    capture_revision: u64,
    cx: &gpui_kit::App,
) -> bool {
    tools
        .command_block_binding(cx)
        .is_some_and(|(session, generation)| {
            session == event.session_id
                && generation == source_generation
                && tools.contains_command_block(event.block_id, cx)
                && capture_revision_matches(
                    Some(capture_revision),
                    tools.command_block_export_revision(event.block_id, cx),
                )
        })
}

/// 持久未知提示只要求同一来源会话代次和 block 仍存在，不依赖不断变化的输出 revision。
fn source_block_is_current(
    source: &Entity<TerminalToolsView>,
    event: &CommandBlockEvent,
    source_generation: u64,
    cx: &gpui_kit::App,
) -> bool {
    let tools = source.read(cx);
    tools
        .command_block_binding(cx)
        .is_some_and(|(session, generation)| {
            session == event.session_id
                && generation == source_generation
                && tools.contains_command_block(event.block_id, cx)
        })
}
