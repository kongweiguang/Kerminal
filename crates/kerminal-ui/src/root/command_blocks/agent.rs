//! Agent 命令块预览、精确目标复核与原子提交流程。
//!
//! @author kongweiguang

use crate::features::terminal_tools::{
    CommandBlockEvent, TerminalToolsView, command_blocks::capture_revision_matches,
};
use gpui_kit::component::WindowExt as _;
use gpui_kit::{AppContext as _, Context, Entity, Window};
use kerminal_application::agents::{
    AgentResumeRecoveryStatus, AgentTextPreview, SendAgentCommandRequest,
};
use kerminal_domain::{
    agents::AgentSessionStatus,
    identity::{AgentSessionId, SessionId},
    sessions::{SessionLifecycle, SessionOwner},
};

use super::super::RootView;
use super::super::terminal_tools_runtime::{TerminalToolsOwner, owner_is_current, resolve_owner};
use super::{RequestOperation, set_notice};

const MAX_AGENT_PREVIEW_BYTES: usize = 32 * 1024;

/// 发起时捕获的同一命令块来源；revision、会话代次和 owner 必须贯穿预览与确认，不能临时重取。
///
/// @author kongweiguang
struct CapturedAgentCommandSource {
    source: Entity<TerminalToolsView>,
    event: CommandBlockEvent,
    source_generation: u64,
    capture_revision: u64,
    owner: TerminalToolsOwner,
}

/// 同一次已核验的 Agent 输入目标；确认与 writer 准入继续复核这些 ID 和 session generation。
///
/// @author kongweiguang
struct CapturedAgentCommandTarget {
    agent_id: AgentSessionId,
    terminal_session_id: SessionId,
    session_generation: u64,
}

/// 先让应用层统一脱敏并限制预览，再复核精确活动 Agent 和输入终端后显示确认框。
///
/// @author kongweiguang
pub(super) fn request_agent_preview(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    event: CommandBlockEvent,
    session_generation: u64,
    cx: &mut Context<RootView>,
) {
    let Some(agent_id) = root.selected_agent_session_id.clone() else {
        set_notice(
            &source,
            &event,
            session_generation,
            Some(
                crate::locale::text("请先选择一个 Agent 会话", "Select an Agent session first")
                    .to_string(),
            ),
            cx,
        );
        return;
    };
    let Some(revision) = event.capture_revision else {
        return;
    };
    let Some(text) = source
        .read(cx)
        .command_block_export_text(event.block_id, cx)
    else {
        return;
    };
    let Some(owner) = resolve_owner(root, &source, &event.session_id, cx) else {
        return;
    };
    let Some(agents) = root.services.agents.clone() else {
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
    };

    let ticket = root.command_block_runtime.begin(
        owner.clone(),
        RequestOperation::AgentPreview(event.block_id),
    );
    let executor = root.services.executor.clone();
    let callback_ticket = ticket.clone();
    let callback_owner = owner;
    let selected_agent = agent_id.clone();
    let source_for_task = source.clone();
    let event_for_task = event.clone();
    let window_handle = root.window_handle;
    let task_cancellation = ticket.cancellation.clone();
    let task = cx.spawn(async move |this, cx| {
        let request_agent = selected_agent.clone();
        let request_text = text;
        let result = executor
            .run_cancellable(task_cancellation, move |operation_cancellation| async move {
                let preview = agents
                    .preview_text(
                        request_agent.clone(),
                        request_text,
                        MAX_AGENT_PREVIEW_BYTES,
                        operation_cancellation.clone(),
                    )
                    .await?;
                let view = agents.get(request_agent, operation_cancellation).await?;
                Ok((preview, view))
            })
            .await;
        let _ = cx.update_window(window_handle, move |_, window, app| {
            let _ = this.update(app, |root, cx| {
                if !root.command_block_runtime.is_current(&callback_ticket) {
                    return;
                }
                if !owner_is_current(root, &callback_owner, cx)
                    || root.selected_agent_session_id.as_ref() != Some(&selected_agent)
                    || !source_revision_is_current(
                        &source_for_task,
                        &event_for_task,
                        session_generation,
                        revision,
                        cx,
                    )
                {
                    root.command_block_runtime.finish(&callback_ticket);
                    return;
                }

                let (preview, view) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        root.command_block_runtime.finish(&callback_ticket);
                        set_notice(
                            &source_for_task,
                            &event_for_task,
                            session_generation,
                            Some(error.public_message().to_owned()),
                            cx,
                        );
                        return;
                    }
                };
                if let Some(recovery) = view.resume_recovery.as_ref() {
                    root.command_block_runtime.finish(&callback_ticket);
                    let message = match recovery.status {
                        AgentResumeRecoveryStatus::Pending => crate::locale::text(
                            "指定 Agent 的恢复提交尚未核验；未打开预览，也未发送内容。请先完成恢复核验。",
                            "The selected Agent has an unverified resume commit; no preview was opened and nothing was sent. Resolve its recovery first.",
                        ),
                        AgentResumeRecoveryStatus::Unknown => crate::locale::text(
                            "指定 Agent 的恢复结果未知；未打开预览，也未发送内容。请先检查该 Agent 终端并完成恢复核验。",
                            "The selected Agent's resume outcome is unknown; no preview was opened and nothing was sent. Inspect that Agent terminal and resolve recovery first.",
                        ),
                    }
                    .to_string();
                    set_notice(
                        &source_for_task,
                        &event_for_task,
                        session_generation,
                        Some(message),
                        cx,
                    );
                    root.status = Some(crate::locale::text(
                        "Agent 恢复结果尚未核验，命令块发送已阻止",
                        "Command-block send was blocked because Agent recovery is unverified",
                    ));
                    cx.notify();
                    return;
                }
                let Some(agent_terminal) = view.terminal.filter(|terminal| {
                    view.session.agent_session_id == selected_agent
                        && view.session.status == AgentSessionStatus::Active
                        && terminal.lifecycle == SessionLifecycle::Running
                        && terminal.owner
                            == (SessionOwner::Agent {
                                agent_session_id: selected_agent.clone(),
                            })
                }) else {
                    root.command_block_runtime.finish(&callback_ticket);
                    set_notice(
                        &source_for_task,
                        &event_for_task,
                        session_generation,
                        Some(
                            crate::locale::text(
                                "目标 Agent 不再活动，未发送内容",
                                "The selected Agent is no longer active; nothing was sent",
                            )
                            .to_string(),
                        ),
                        cx,
                    );
                    return;
                };

                if root.agent_terminal_session_id.as_ref() != Some(&agent_terminal.session_id) {
                    root.command_block_runtime.finish(&callback_ticket);
                    return;
                }
                let Some(binding) = root.pane_input_handlers.get(&agent_terminal.session_id) else {
                    root.command_block_runtime.finish(&callback_ticket);
                    set_notice(
                        &source_for_task,
                        &event_for_task,
                        session_generation,
                        Some(
                            crate::locale::text(
                                "Agent 终端输入绑定尚未就绪",
                                "The Agent terminal input binding is not ready",
                            )
                            .to_string(),
                        ),
                        cx,
                    );
                    return;
                };
                if binding.binding.session_id != agent_terminal.session_id
                    || binding.binding.owner
                        != (SessionOwner::Agent {
                            agent_session_id: selected_agent.clone(),
                        })
                {
                    root.command_block_runtime.finish(&callback_ticket);
                    return;
                }

                let previous_submission_unknown = root
                    .command_block_runtime
                    .agent_submission_is_uncertain(
                        &callback_owner.session_id,
                        event_for_task.block_id,
                        &selected_agent,
                    );
                if previous_submission_unknown && source_block_is_current(
                    &source_for_task,
                    &event_for_task,
                    session_generation,
                    cx,
                ) {
                    source_for_task.update(cx, |tools, cx| {
                        tools.mark_agent_submission_unknown(
                            &event_for_task.session_id,
                            session_generation,
                            event_for_task.block_id,
                            selected_agent.clone(),
                            cx,
                        );
                    });
                }

                root.command_block_runtime.finish(&callback_ticket);
                open_agent_preview_dialog(
                    CapturedAgentCommandSource {
                        source: source_for_task,
                        event: event_for_task,
                        source_generation: session_generation,
                        capture_revision: revision,
                        owner: callback_owner,
                    },
                    CapturedAgentCommandTarget {
                        agent_id: selected_agent,
                        terminal_session_id: agent_terminal.session_id,
                        session_generation: binding.binding.session_generation,
                    },
                    preview,
                    previous_submission_unknown,
                    window,
                    cx,
                );
            });
        });
    });
    root.command_block_runtime.install_task(&ticket, task);
}

/// 展示应用层返回的确切预览；用户确认后再次核对来源和 Agent 终端 binding。
///
/// @author kongweiguang
fn open_agent_preview_dialog(
    source_context: CapturedAgentCommandSource,
    target_context: CapturedAgentCommandTarget,
    preview: AgentTextPreview,
    previous_submission_unknown: bool,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let CapturedAgentCommandSource {
        source,
        event,
        source_generation,
        capture_revision,
        owner: source_owner,
    } = source_context;
    let CapturedAgentCommandTarget {
        agent_id,
        terminal_session_id: agent_terminal_session_id,
        session_generation: agent_terminal_generation,
    } = target_context;
    let title = crate::locale::text("命令块 Agent 预览", "Command-block Agent preview");
    let redaction_notice = if preview.redacted {
        crate::locale::text("\n[敏感字段已脱敏]", "\n[secret fields redacted]")
    } else {
        "".into()
    };
    let truncation_notice = if preview.truncated {
        crate::locale::text(
            "\n[预览已按 32 KiB 限制截断；只会提交下方显示的文本]",
            "\n[Preview was capped at 32 KiB; only the text shown below will be submitted]",
        )
    } else {
        "".into()
    };
    let unknown_notice = if previous_submission_unknown {
        crate::locale::text(
            format!(
                "\n\n上次向 Agent {agent_id} 提交的结果仍未知，本次发送可能重复。请先检查指定 Agent 终端。"
            ),
            format!(
                "\n\nThe previous submission to Agent {agent_id} is still unknown; this send may duplicate it. Inspect the selected Agent terminal first."
            ),
        )
    } else {
        "".into()
    };
    let description = format!(
        "{}: {}\n{} {}\n\n{}\n{}{}{}{}",
        crate::locale::text("目标 Agent", "Target Agent"),
        agent_id,
        crate::locale::text("Agent terminal", "Agent terminal"),
        agent_terminal_session_id,
        crate::locale::text(
            "确认后将原样提交以下脱敏文本；writer ACK 不表示 CLI 已执行",
            "Confirming submits the exact redacted text below; writer ACK does not mean the CLI executed it",
        ),
        preview.text,
        redaction_notice,
        truncation_notice,
        unknown_notice,
    );
    let root_weak = cx.weak_entity();
    let confirm_source = source.clone();
    let cancel_source = source;
    let confirm_event = event.clone();
    let cancel_event = event;
    let confirm_agent = agent_id.clone();
    let cancel_agent = agent_id;
    let confirm_owner = source_owner.clone();
    let confirm_preview = preview.clone();
    let confirm_unknown_acknowledged = previous_submission_unknown;
    let confirm_label = if previous_submission_unknown {
        crate::locale::text(
            "已检查，仍提交本次文本",
            "I checked; submit this text anyway",
        )
    } else {
        crate::locale::text("提交本次文本", "Submit this text")
    };
    let cancel_message = if previous_submission_unknown {
        crate::locale::text(
            format!(
                "上次向 Agent {cancel_agent} 的提交结果仍未知；已取消本次提交。"
            ),
            format!(
                "The previous submission to Agent {cancel_agent} is still unknown; this submission was cancelled."
            ),
        )
        .to_string()
    } else {
        crate::locale::text(
            format!("已取消发送到 Agent {cancel_agent}"),
            format!("Cancelled sending to Agent {cancel_agent}"),
        )
        .to_string()
    };

    window.open_alert_dialog(cx, move |alert, _, _| {
        let root_weak_for_callback = root_weak.clone();
        let confirm_source_for_callback = confirm_source.clone();
        let confirm_event_for_callback = confirm_event.clone();
        let confirm_agent_for_callback = confirm_agent.clone();
        let confirm_owner_for_callback = confirm_owner.clone();
        let confirm_preview_for_callback = confirm_preview.clone();
        let confirm_unknown_for_callback = confirm_unknown_acknowledged;
        let cancel_source_for_callback = cancel_source.clone();
        let cancel_event_for_callback = cancel_event.clone();
        let cancel_message_for_callback = cancel_message.clone();
        let terminal_session_id_for_callback = agent_terminal_session_id.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .confirm()
            .ok_text(confirm_label.clone())
            .on_ok(move |_, _, app| {
                let source = confirm_source_for_callback.clone();
                let event = confirm_event_for_callback.clone();
                let owner = confirm_owner_for_callback.clone();
                let agent = confirm_agent_for_callback.clone();
                let terminal_session_id = terminal_session_id_for_callback.clone();
                let preview = confirm_preview_for_callback.clone();
                let _ = root_weak_for_callback.update(app, move |root, cx| {
                    begin_agent_submission(
                        root,
                        CapturedAgentCommandSource {
                            source,
                            event,
                            source_generation,
                            capture_revision,
                            owner,
                        },
                        CapturedAgentCommandTarget {
                            agent_id: agent,
                            terminal_session_id,
                            session_generation: agent_terminal_generation,
                        },
                        preview,
                        confirm_unknown_for_callback,
                        cx,
                    );
                });
                true
            })
            .on_cancel(move |_, _, app| {
                cancel_source_for_callback.update(app, |tools, cx| {
                    if source_binding_matches(
                        tools,
                        &cancel_event_for_callback,
                        source_generation,
                        capture_revision,
                        cx,
                    ) {
                        tools.set_command_block_notice(
                            &cancel_event_for_callback.session_id,
                            source_generation,
                            cancel_event_for_callback.block_id,
                            Some(cancel_message_for_callback.clone()),
                            cx,
                        );
                    }
                });
                true
            })
    });
}

/// 在确认回调中同时复核 source revision、活动 Agent、输入 owner 和 session generation。
///
/// @author kongweiguang
fn begin_agent_submission(
    root: &mut RootView,
    source_context: CapturedAgentCommandSource,
    target_context: CapturedAgentCommandTarget,
    preview: AgentTextPreview,
    unknown_warning_acknowledged: bool,
    cx: &mut Context<RootView>,
) {
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
        return;
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
        return;
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
        return;
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
        return;
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
        return;
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
        return;
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
        return;
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
