//! OSC 133 命令块查询与显式用户动作的 Root 协调器。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui_kit::{ClipboardItem, Context, Entity, EntityId, Image, ImageFormat, Task, Window};
use kerminal_application::sessions::{
    CommandBlockOutputIncompleteReason, CommandBlockOutputSnapshot, CommandBlockOutputState,
    CommandBlockSnapshot, MAX_COMMAND_BLOCK_OUTPUT_BYTES,
};
use kerminal_domain::identity::SessionId;
use tokio_util::sync::CancellationToken;

use crate::features::terminal_tools::command_blocks::capture_revision_matches;
use crate::features::terminal_tools::{
    CommandBlockAction, CommandBlockContent, CommandBlockContentUnavailable, CommandBlockEvent,
    CommandBlockProjection, TerminalToolsEvent, TerminalToolsView,
};

mod agent;

use super::RootView;
use super::terminal_tools_runtime::{TerminalToolsOwner, owner_is_current, resolve_owner};

/// 每个视图操作保留自己的取消令牌和 GPUI task，迟到回执不能覆盖新操作。
#[derive(Default)]
pub(super) struct CommandBlockRuntime {
    next_generation: u64,
    active: HashMap<RequestKey, ActiveRequest>,
    uncertain_agent_submissions: HashSet<UncertainAgentSubmission>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RequestKey {
    source_id: EntityId,
    session_id: SessionId,
    operation: RequestOperation,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RequestOperation {
    Snapshot,
    Transcript(kerminal_application::sessions::CommandBlockId),
    ImageExport(kerminal_application::sessions::CommandBlockId),
    AgentPreview(kerminal_application::sessions::CommandBlockId),
    AgentSend(kerminal_application::sessions::CommandBlockId),
}

struct ActiveRequest {
    owner: TerminalToolsOwner,
    generation: u64,
    cancellation: CancellationToken,
    _task: Option<Task<()>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct UncertainAgentSubmission {
    session_id: SessionId,
    block_id: kerminal_application::sessions::CommandBlockId,
    agent_session_id: kerminal_domain::identity::AgentSessionId,
}

#[derive(Clone)]
struct RequestTicket {
    key: RequestKey,
    owner: TerminalToolsOwner,
    generation: u64,
    cancellation: CancellationToken,
}

impl CommandBlockRuntime {
    /// 新请求取代同一视图/区块的旧任务，并取消应用用例与 GPUI 等待双方。
    fn begin(&mut self, owner: TerminalToolsOwner, operation: RequestOperation) -> RequestTicket {
        let key = RequestKey {
            source_id: owner.source_id,
            session_id: owner.session_id.clone(),
            operation,
        };
        if let Some(previous) = self.active.remove(&key) {
            previous.cancellation.cancel();
        }

        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        let cancellation = CancellationToken::new();
        self.active.insert(
            key.clone(),
            ActiveRequest {
                owner: owner.clone(),
                generation,
                cancellation: cancellation.clone(),
                _task: None,
            },
        );
        RequestTicket {
            key,
            owner,
            generation,
            cancellation,
        }
    }

    /// Agent Paste+Enter 不是幂等操作；同一操作仍在运行时拒绝第二次提交。
    fn begin_exclusive(
        &mut self,
        owner: TerminalToolsOwner,
        operation: RequestOperation,
    ) -> Option<RequestTicket> {
        if self
            .active
            .keys()
            .any(|key| key.session_id == owner.session_id && key.operation == operation)
        {
            return None;
        }
        Some(self.begin(owner, operation))
    }

    /// 输入 ACK 未知时按来源会话、命令块和目标 Agent 留存风险，不把源输出变化当作回执。
    fn mark_agent_submission_uncertain(
        &mut self,
        session_id: &SessionId,
        block_id: kerminal_application::sessions::CommandBlockId,
        agent_session_id: kerminal_domain::identity::AgentSessionId,
    ) {
        self.uncertain_agent_submissions
            .insert(UncertainAgentSubmission {
                session_id: session_id.clone(),
                block_id,
                agent_session_id,
            });
    }

    /// 精确查询本来源区块发往某个 Agent 的历史未知结果，与后续 transcript revision 无关。
    fn agent_submission_is_uncertain(
        &self,
        session_id: &SessionId,
        block_id: kerminal_application::sessions::CommandBlockId,
        agent_session_id: &kerminal_domain::identity::AgentSessionId,
    ) -> bool {
        self.uncertain_agent_submissions
            .contains(&UncertainAgentSubmission {
                session_id: session_id.clone(),
                block_id,
                agent_session_id: agent_session_id.clone(),
            })
    }

    /// 提交尚在进行时不再创建新预览，避免重复确认框竞争同一非幂等操作。
    fn agent_submission_is_active(
        &self,
        owner: &TerminalToolsOwner,
        block_id: kerminal_application::sessions::CommandBlockId,
    ) -> bool {
        self.active.keys().any(|key| {
            key.session_id == owner.session_id
                && key.operation == RequestOperation::AgentSend(block_id)
        })
    }

    /// 完成一次非幂等写入后释放 in-flight 标记，不清理未知结果的独立锁。
    fn finish(&mut self, ticket: &RequestTicket) {
        if self.is_current(ticket) {
            self.active.remove(&ticket.key);
        }
    }

    /// 保存 task 所有权；ticket 已被替换时立即释放失效任务。
    fn install_task(&mut self, ticket: &RequestTicket, task: Task<()>) {
        if let Some(active) = self.current_mut(ticket) {
            active._task = Some(task);
        }
    }

    /// 比较 owner 与单调请求代次，阻断旧实体、旧 pane 和旧 block 的回执。
    fn is_current(&self, ticket: &RequestTicket) -> bool {
        self.active.get(&ticket.key).is_some_and(|active| {
            active.generation == ticket.generation && active.owner == ticket.owner
        })
    }

    /// 关闭来源时取消同会话查询；非幂等 Agent 提交保留 ticket 直到收到最终回执。
    fn cancel_source_session(&mut self, source_id: EntityId, session_id: &SessionId) {
        self.active.retain(|key, active| {
            let keep = key.source_id != source_id || &key.session_id != session_id;
            if !keep {
                active.cancellation.cancel();
            }
            // Agent 提交即使收到取消也必须等用例返回真实 ACK/未知结果，不能丢弃非幂等写入。
            keep || matches!(key.operation, RequestOperation::AgentSend(_))
        });
    }

    /// 视图重绑定到另一个会话时，取消原会话的读取，避免同一 Entity 留下旧任务。
    fn cancel_other_sessions(&mut self, source_id: EntityId, session_id: &SessionId) {
        self.active.retain(|key, active| {
            let keep = key.source_id != source_id || &key.session_id == session_id;
            if !keep {
                active.cancellation.cancel();
            }
            // 被复用的工具实体不再拥有旧操作，但保留提交 ticket 以接收其最终结果。
            keep || matches!(key.operation, RequestOperation::AgentSend(_))
        });
    }

    /// 新权威快照收缩卡片集合后，释放不再可见命令块的查询与操作任务。
    fn retain_live_blocks(
        &mut self,
        source_id: EntityId,
        session_id: &SessionId,
        live_blocks: &HashSet<kerminal_application::sessions::CommandBlockId>,
    ) {
        self.active.retain(|key, active| {
            let keep = if key.source_id != source_id || &key.session_id != session_id {
                true
            } else {
                match key.operation {
                    RequestOperation::Snapshot => true,
                    RequestOperation::Transcript(block_id)
                    | RequestOperation::ImageExport(block_id)
                    | RequestOperation::AgentPreview(block_id)
                    | RequestOperation::AgentSend(block_id) => live_blocks.contains(&block_id),
                }
            };
            if !keep {
                active.cancellation.cancel();
            }
            // block 从快照移除时也不能丢失正在途中的 Paste+Enter 回执。
            keep || matches!(key.operation, RequestOperation::AgentSend(_))
        });
        self.uncertain_agent_submissions.retain(|entry| {
            entry.session_id != *session_id || live_blocks.contains(&entry.block_id)
        });
    }

    /// 修改活跃请求前再次按 generation 和 owner 定位，防止清理新一轮同键任务。
    fn current_mut(&mut self, ticket: &RequestTicket) -> Option<&mut ActiveRequest> {
        self.active
            .get_mut(&ticket.key)
            .filter(|active| active.generation == ticket.generation && active.owner == ticket.owner)
    }
}

/// 读取明示 session 的权威 bounded 命令块快照；订阅通知只触发重新查询。
pub(super) fn refresh_from_event(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    session_id: SessionId,
    cx: &mut Context<RootView>,
) {
    let Some(owner) = resolve_owner(root, &source, &session_id, cx) else {
        root.command_block_runtime
            .cancel_source_session(source.entity_id(), &session_id);
        return;
    };
    root.command_block_runtime
        .cancel_other_sessions(source.entity_id(), &session_id);
    let ticket = root
        .command_block_runtime
        .begin(owner.clone(), RequestOperation::Snapshot);
    let sessions = root.services.sessions.clone();
    let executor = root.services.executor.clone();
    let callback_ticket = ticket.clone();
    let callback_owner = owner;
    let query_session = session_id.clone();
    let task_cancellation = ticket.cancellation.clone();

    let task = cx.spawn(async move |this, cx| {
        let service_session = query_session.clone();
        let result = executor
            .run_cancellable(
                task_cancellation,
                move |operation_cancellation| async move {
                    sessions
                        .command_block_snapshot(service_session, &operation_cancellation)
                        .await
                },
            )
            .await;
        let _ = this.update(cx, |root, cx| {
            if !root.command_block_runtime.is_current(&callback_ticket) {
                return;
            }
            if !owner_is_current(root, &callback_owner, cx) {
                return;
            }
            match result {
                Ok(snapshot) if snapshot_matches_session(&snapshot, &query_session) => {
                    let session_generation = snapshot.session_generation;
                    let revision = snapshot.revision;
                    let blocks = project_blocks(snapshot);
                    source.update(cx, |tools, cx| {
                        tools.set_command_blocks(
                            &query_session,
                            session_generation,
                            revision,
                            blocks,
                            cx,
                        );
                    });
                    let live_blocks = source
                        .read(cx)
                        .command_block_ids(cx)
                        .into_iter()
                        .collect::<HashSet<_>>();
                    root.command_block_runtime.retain_live_blocks(
                        source.entity_id(),
                        &query_session,
                        &live_blocks,
                    );
                    let expanded_blocks = source.read(cx).expanded_command_block_ids(cx);
                    for block_id in expanded_blocks {
                        request_transcript(
                            root,
                            callback_owner.clone(),
                            source.clone(),
                            query_session.clone(),
                            session_generation,
                            block_id,
                            cx,
                        );
                    }
                }
                Ok(_) => {
                    let message = crate::locale::text(
                        "命令块快照会话身份不匹配",
                        "Command-block snapshot belongs to a different session",
                    )
                    .to_string();
                    source.update(cx, |tools, cx| {
                        tools.set_command_block_snapshot_error(&query_session, message, cx)
                    });
                }
                Err(error) => {
                    let message = error.public_message().to_owned();
                    source.update(cx, |tools, cx| {
                        tools.set_command_block_snapshot_error(&query_session, message.clone(), cx)
                    });
                    root.status = Some(message.into());
                    cx.notify();
                }
            }
        });
    });
    root.command_block_runtime.install_task(&ticket, task);
}

/// 只处理命令块事件并返回是否消费，Root 其余工具事件仍交给原 dispatcher。
pub(super) fn handle_event_from(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    event: &TerminalToolsEvent,
    _window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    let TerminalToolsEvent::CommandBlockAction {
        session_id,
        block_id,
        capture_revision,
        action,
    } = event
    else {
        return false;
    };
    let event = CommandBlockEvent {
        session_id: session_id.clone(),
        block_id: *block_id,
        capture_revision: *capture_revision,
        action: *action,
    };
    handle_action(root, source, event, cx);
    true
}

/// 先验证视图 owner 与 block generation，再在 UI 线程只执行轻量剪贴板/状态操作。
fn handle_action(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    event: CommandBlockEvent,
    cx: &mut Context<RootView>,
) {
    let Some(owner) = resolve_owner(root, &source, &event.session_id, cx) else {
        return;
    };
    let Some((bound_session, generation)) = source.read(cx).command_block_binding(cx) else {
        return;
    };
    if bound_session != event.session_id
        || !source.read(cx).contains_command_block(event.block_id, cx)
        || source
            .read(cx)
            .command_block_export_text(event.block_id, cx)
            .is_none()
            && !matches!(event.action, CommandBlockAction::ToggleExpanded { .. })
    {
        return;
    }

    if !matches!(event.action, CommandBlockAction::ToggleExpanded { .. })
        && !capture_revision_matches(
            event.capture_revision,
            source
                .read(cx)
                .command_block_export_revision(event.block_id, cx),
        )
    {
        set_notice(
            &source,
            &event,
            generation,
            Some(
                crate::locale::text(
                    "命令输出已更新，请重新选择操作",
                    "Command output changed; choose the action again",
                )
                .to_string(),
            ),
            cx,
        );
        return;
    }

    if matches!(event.action, CommandBlockAction::SendToAgent)
        && root
            .command_block_runtime
            .agent_submission_is_active(&owner, event.block_id)
    {
        set_notice(
            &source,
            &event,
            generation,
            Some(
                crate::locale::text(
                    "此命令块正在提交，请等待当前结果",
                    "This command block is being submitted; wait for its result",
                )
                .to_string(),
            ),
            cx,
        );
        return;
    }

    match event.action {
        CommandBlockAction::ToggleExpanded { expanded } => {
            if expanded {
                source.update(cx, |tools, cx| {
                    tools.set_command_block_content(
                        &event.session_id,
                        generation,
                        event.block_id,
                        CommandBlockContent::Loading,
                        cx,
                    );
                });
                request_transcript(
                    root,
                    owner,
                    source,
                    event.session_id,
                    generation,
                    event.block_id,
                    cx,
                );
            } else {
                root.command_block_runtime.active.retain(|key, active| {
                    let keep = key.source_id != owner.source_id
                        || key.session_id != event.session_id
                        || key.operation != RequestOperation::Transcript(event.block_id);
                    if !keep {
                        active.cancellation.cancel();
                    }
                    keep
                });
            }
        }
        CommandBlockAction::CopyText => {
            let Some(text) = source
                .read(cx)
                .command_block_export_text(event.block_id, cx)
            else {
                return;
            };
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            root.status = Some(crate::locale::text(
                "已将命令块文本交给系统剪贴板",
                "Command-block text was sent to the system clipboard",
            ));
            set_notice(&source, &event, generation, None, cx);
            cx.notify();
        }
        CommandBlockAction::CopyImage => {
            let Some(text) = source
                .read(cx)
                .command_block_export_text(event.block_id, cx)
            else {
                return;
            };
            request_image_export(root, owner, source, event, generation, text, cx);
        }
        CommandBlockAction::SendToAgent => {
            agent::request_agent_preview(root, source, event, generation, cx);
        }
    }
}

/// 只为当前展开的命令块读取有界 transcript，任务同时受 session owner 与 UI 请求代次约束。
fn request_transcript(
    root: &mut RootView,
    owner: TerminalToolsOwner,
    source: Entity<TerminalToolsView>,
    session_id: SessionId,
    session_generation: u64,
    block_id: kerminal_application::sessions::CommandBlockId,
    cx: &mut Context<RootView>,
) {
    let Some((bound_session, bound_generation)) = source.read(cx).command_block_binding(cx) else {
        return;
    };
    if owner.session_id != session_id
        || bound_session != session_id
        || bound_generation != session_generation
        || !source.read(cx).contains_command_block(block_id, cx)
        || !source.read(cx).command_block_is_expanded(block_id, cx)
    {
        return;
    }

    let ticket = root
        .command_block_runtime
        .begin(owner.clone(), RequestOperation::Transcript(block_id));
    let sessions = root.services.sessions.clone();
    let executor = root.services.executor.clone();
    let callback_ticket = ticket.clone();
    let callback_owner = owner;
    let callback_source = source.clone();
    let query_session = session_id.clone();
    let task_cancellation = ticket.cancellation.clone();

    let task = cx.spawn(async move |this, cx| {
        let service_session = query_session.clone();
        let result = executor
            .run_cancellable(
                task_cancellation,
                move |operation_cancellation| async move {
                    sessions
                        .command_block_output(
                            service_session,
                            block_id,
                            MAX_COMMAND_BLOCK_OUTPUT_BYTES,
                            &operation_cancellation,
                        )
                        .await
                },
            )
            .await;
        let _ = this.update(cx, |root, cx| {
            if !root.command_block_runtime.is_current(&callback_ticket)
                || !owner_is_current(root, &callback_owner, cx)
            {
                return;
            }
            let Some((bound_session, bound_generation)) =
                callback_source.read(cx).command_block_binding(cx)
            else {
                return;
            };
            if bound_session != query_session
                || bound_generation != session_generation
                || !callback_source
                    .read(cx)
                    .contains_command_block(block_id, cx)
                || !callback_source
                    .read(cx)
                    .command_block_is_expanded(block_id, cx)
            {
                return;
            }

            match result {
                Ok(snapshot) => {
                    let Some(content) = project_command_block_output(
                        snapshot,
                        &query_session,
                        session_generation,
                        block_id,
                    ) else {
                        return;
                    };
                    callback_source.update(cx, |tools, cx| {
                        tools.set_command_block_content(
                            &query_session,
                            session_generation,
                            block_id,
                            content,
                            cx,
                        );
                    });
                }
                Err(error) => {
                    callback_source.update(cx, |tools, cx| {
                        tools.set_command_block_content(
                            &query_session,
                            session_generation,
                            block_id,
                            CommandBlockContent::Unavailable(
                                CommandBlockContentUnavailable::QueryFailed(
                                    error.public_message().to_owned(),
                                ),
                            ),
                            cx,
                        );
                    });
                }
            }
            cx.notify();
        });
    });
    root.command_block_runtime.install_task(&ticket, task);
}

/// 将应用层 transcript 状态映射到卡片状态，并拒绝跨 session、block 或重连代次的回执。
fn project_command_block_output(
    snapshot: CommandBlockOutputSnapshot,
    session_id: &SessionId,
    session_generation: u64,
    block_id: kerminal_application::sessions::CommandBlockId,
) -> Option<CommandBlockContent> {
    if &snapshot.session_id != session_id
        || snapshot.block_id != block_id
        || block_id.session_generation != session_generation
    {
        return None;
    }

    let capture_revision = snapshot.capture_revision;
    let text = snapshot.text.map(|text| text.as_str().to_owned());
    let truncated = snapshot.truncated;
    Some(match snapshot.state {
        CommandBlockOutputState::NotCaptured => {
            CommandBlockContent::Unavailable(CommandBlockContentUnavailable::NotCaptured)
        }
        CommandBlockOutputState::Capturing => match text {
            Some(text) => CommandBlockContent::Available {
                text,
                capture_revision,
                truncated,
                complete: false,
            },
            None => CommandBlockContent::Loading,
        },
        CommandBlockOutputState::Complete { .. } => match text {
            Some(text) => CommandBlockContent::Available {
                text,
                capture_revision,
                truncated,
                complete: true,
            },
            None => {
                CommandBlockContent::Unavailable(CommandBlockContentUnavailable::CaptureUnavailable)
            }
        },
        CommandBlockOutputState::Incomplete(reason) => match text {
            Some(text) => CommandBlockContent::Incomplete {
                text,
                capture_revision,
                reason: incomplete_output_label(reason),
                truncated,
            },
            None => CommandBlockContent::Unavailable(CommandBlockContentUnavailable::Incomplete(
                incomplete_output_label(reason),
            )),
        },
        CommandBlockOutputState::Evicted => {
            CommandBlockContent::Unavailable(CommandBlockContentUnavailable::Evicted)
        }
    })
}

/// 保留真实采集终止原因，断线、关闭和重连不会被显示成完整结果。
fn incomplete_output_label(reason: CommandBlockOutputIncompleteReason) -> String {
    match reason {
        CommandBlockOutputIncompleteReason::Superseded => {
            crate::locale::text("后续命令取代了当前采集", "Superseded by a later command")
                .to_string()
        }
        CommandBlockOutputIncompleteReason::Closed => {
            crate::locale::text("会话已关闭", "Session closed").to_string()
        }
        CommandBlockOutputIncompleteReason::Disconnected => {
            crate::locale::text("连接已断开", "Connection disconnected").to_string()
        }
        CommandBlockOutputIncompleteReason::Reconnected => {
            crate::locale::text("会话已重连", "Session reconnected").to_string()
        }
    }
}

/// 在 GPUI 后台 executor 做有限 PNG 栅格化；失败时将同一 bounded 文本交给剪贴板。
fn request_image_export(
    root: &mut RootView,
    owner: TerminalToolsOwner,
    source: Entity<TerminalToolsView>,
    event: CommandBlockEvent,
    session_generation: u64,
    text: String,
    cx: &mut Context<RootView>,
) {
    let ticket = root
        .command_block_runtime
        .begin(owner.clone(), RequestOperation::ImageExport(event.block_id));
    let background = cx.background_executor().clone();
    let callback_ticket = ticket.clone();
    let callback_owner = owner;
    let callback_source = source.clone();
    let callback_event = event.clone();
    let task = cx.spawn(async move |this, cx| {
        let render_text = text.clone();
        let encoded = background
            .spawn(async move {
                let renderer = gpui_kit::SvgRenderer::new(Arc::new(()));
                crate::features::terminal_tools::command_blocks::image_export::encode_command_block_png(
                    &render_text,
                    &renderer,
                )
            })
            .await;
        let _ = this.update(cx, |root, cx| {
            if !root.command_block_runtime.is_current(&callback_ticket) {
                return;
            }
            if !owner_is_current(root, &callback_owner, cx) {
                return;
            }
            callback_source.update(cx, |tools, cx| {
                if tools.command_block_binding(cx).is_none_or(|(session, generation)| {
                    session != callback_event.session_id || generation != session_generation
                }) || !capture_revision_matches(
                    callback_event.capture_revision,
                    tools.command_block_export_revision(callback_event.block_id, cx),
                ) {
                    return;
                }
                match encoded {
                    Ok(png) => {
                        let image = Image::from_bytes(ImageFormat::Png, png);
                        cx.write_to_clipboard(ClipboardItem::new_image(&image));
                        tools.set_command_block_notice(
                            &callback_event.session_id,
                            session_generation,
                            callback_event.block_id,
                            Some(
                                crate::locale::text(
                                    "已将 PNG 图像交给系统剪贴板",
                                    "PNG image was sent to the system clipboard",
                                )
                                .to_string(),
                            ),
                            cx,
                        );
                    }
                    Err(_) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        tools.set_command_block_notice(
                            &callback_event.session_id,
                            session_generation,
                            callback_event.block_id,
                            Some(
                                crate::locale::text(
                                    "图片生成失败，已回退复制文本",
                                    "Image generation failed; copied text instead",
                                )
                                .to_string(),
                            ),
                            cx,
                        );
                    }
                }
            });
            cx.notify();
        });
    });
    root.command_block_runtime.install_task(&ticket, task);
}

/// 只在当前 generation 的工具卡片写入操作回执，旧 block 或 session 已被替换时静默丢弃。
fn set_notice(
    source: &Entity<TerminalToolsView>,
    event: &CommandBlockEvent,
    generation: u64,
    message: Option<String>,
    cx: &mut Context<RootView>,
) {
    source.update(cx, |tools, cx| {
        tools.set_command_block_notice(&event.session_id, generation, event.block_id, message, cx);
    });
}

/// 将领域快照转换为 UI 状态；终端正文必须通过单独 transcript 内容端口读取。
fn project_blocks(snapshot: CommandBlockSnapshot) -> Vec<CommandBlockProjection> {
    snapshot
        .blocks
        .into_iter()
        .map(|block| CommandBlockProjection {
            id: block.id,
            command: block.command,
            state: block.state,
            content: CommandBlockContent::NotRequested,
        })
        .collect()
}

/// 把显式外部工具事件映射为当前 source/session 的查询边界，测试不需要 GPUI 窗口。
fn snapshot_matches_session(snapshot: &CommandBlockSnapshot, session_id: &SessionId) -> bool {
    if &snapshot.session_id != session_id {
        return false;
    }
    let mut seen = HashSet::with_capacity(snapshot.blocks.len());
    snapshot.blocks.iter().all(|block| {
        block.id.session_generation == snapshot.session_generation && seen.insert(block.id)
    })
}

#[cfg(test)]
#[path = "../../tests/unit/root_command_blocks.rs"]
mod tests;
