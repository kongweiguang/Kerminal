//! 终端输入提交后的安全历史捕获。
//!
//! History records prove that a complete line was accepted by the terminal writer;
//! they do not claim that the shell finished executing it.
//!
//! @author kongweiguang

use kerminal_domain::{
    commands::{
        CommandHistoryRecordRequest, CommandHistoryRecordResult, CommandHistorySource,
        NormalizedCommandHistory,
    },
    connections::RemoteShellDialect,
    identity::SessionId,
    sessions::{SessionInfo, SessionLifecycle, SessionOwner},
};
use kerminal_terminal::input::{KeyPhase, TerminalInputEvent, TerminalKey};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult, PublicErrorCode,
    sessions::{
        TerminalCommandAvailability, TerminalCommandContext, TerminalCommandContextVersion,
        TerminalWriteAdmission, UiInputProjection, UiSessionBinding,
        UiTerminalCommandSubmissionRequest, UiTerminalInputRequest,
    },
};

use super::{CommandsService, execution};

/// 用户以 GPUI 的可信投影提交一行；command 只由 Root 在 Enter 时提供。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiTerminalSubmissionRequest {
    pub input: UiTerminalInputRequest,
    pub command: Option<String>,
    pub record_history: bool,
}

/// 输入成功与历史持久化状态分开返回，防止历史故障导致已写入命令被重放。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalInputSubmissionReceipt {
    pub history_record: Option<CommandHistoryRecordResult>,
    pub history_error_code: Option<PublicErrorCode>,
}

/// UI 的输入 ack 与历史状态分开返回；调用方用同一 context 推进 FIFO 和投影。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiTerminalInputSubmissionResult {
    pub context: TerminalCommandContext,
    pub history: TerminalInputSubmissionReceipt,
}

impl CommandsService {
    /// 通过唯一 SessionService 写入事件并返回真实 ack context，只有可信 Enter 才追加 User 历史。
    ///
    /// @author kongweiguang
    pub async fn submit_ui_terminal_input(
        &self,
        request: UiTerminalSubmissionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<UiTerminalInputSubmissionResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端输入已取消"));
        }
        submit_ui_terminal_input(self, request, cancellation).await
    }

    /// 以 SessionService 的单次命令提交保留 OSC 命令块关联，并要求同一 writer receipt 证明后才追加历史。
    ///
    /// @author kongweiguang
    pub async fn submit_ui_terminal_command(
        &self,
        input: UiTerminalInputRequest,
        command: String,
        before: TerminalCommandContext,
        record_history: bool,
        cancellation: CancellationToken,
    ) -> AppResult<UiTerminalInputSubmissionResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端命令提交已取消"));
        }

        let capture = is_ui_history_candidate(&input, &command, record_history);
        let session = if capture {
            self.session_info(input.binding.session_id.clone())
                .await
                .ok()
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端命令提交已取消"));
        }

        // 命令正文与 Enter 仍由同一个 owner/generation writer gate 原子准入和 ACK。
        let acknowledgement = self
            .sessions
            .write_ui_command_submission_with_receipt(UiTerminalCommandSubmissionRequest {
                input: input.clone(),
                command: command.clone(),
            })
            .await?;
        let history = match (capture, session, acknowledgement.command_associated) {
            (true, Some(session), true) if !cancellation.is_cancelled() => {
                record_ui_terminal_submission_history_after_ack(
                    self,
                    input,
                    command,
                    before,
                    session,
                    acknowledgement.context.clone(),
                    acknowledgement.command_associated,
                )
                .await
            }
            _ => TerminalInputSubmissionReceipt::default(),
        };
        Ok(UiTerminalInputSubmissionResult {
            context: acknowledgement.context,
            history,
        })
    }

    /// 保留 MCP write 的原始字节语义，只有同一次完整 writer ack 的 admission 能补录 Tool 历史。
    pub async fn write_mcp_terminal_input(
        &self,
        session_id: SessionId,
        data: String,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalInputSubmissionReceipt> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端输入已取消"));
        }
        write_mcp_terminal_input(self, session_id, data).await
    }
}

/// UI 路径在写入前固定可信 context 与会话元数据，ACK 后复用统一记录门禁；写失败不触碰历史仓储。
///
/// @author kongweiguang
async fn submit_ui_terminal_input(
    commands: &CommandsService,
    request: UiTerminalSubmissionRequest,
    cancellation: CancellationToken,
) -> AppResult<UiTerminalInputSubmissionResult> {
    let capture = request.command.as_deref().and_then(|command| {
        is_ui_history_candidate(&request.input, command, request.record_history)
            .then_some(command.to_owned())
    });

    let before = if capture.is_some() {
        commands
            .sessions
            .terminal_command_context(request.input.binding.session_id.clone())
            .await
            .ok()
    } else {
        None
    };
    let session = if before.is_some() {
        commands
            .session_info(request.input.binding.session_id.clone())
            .await
            .ok()
    } else {
        None
    };
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("终端输入已取消"));
    }

    let (acknowledgement, command_associated) = if let Some(command) = capture.as_ref() {
        let receipt = commands
            .sessions
            .write_ui_command_submission_with_receipt(UiTerminalCommandSubmissionRequest {
                input: request.input.clone(),
                command: command.clone(),
            })
            .await?;
        (receipt.context, receipt.command_associated)
    } else {
        (
            commands
                .sessions
                .write_ui_input(request.input.clone())
                .await?,
            false,
        )
    };
    if cancellation.is_cancelled() {
        return Ok(UiTerminalInputSubmissionResult {
            context: acknowledgement,
            history: TerminalInputSubmissionReceipt::default(),
        });
    }
    let Some(command) = capture else {
        return Ok(UiTerminalInputSubmissionResult {
            context: acknowledgement,
            history: TerminalInputSubmissionReceipt::default(),
        });
    };
    let (Some(before), Some(session)) = (before, session) else {
        return Ok(UiTerminalInputSubmissionResult {
            context: acknowledgement,
            history: TerminalInputSubmissionReceipt::default(),
        });
    };
    let history = record_ui_terminal_submission_history_after_ack(
        commands,
        request.input,
        command,
        before,
        session,
        acknowledgement.clone(),
        command_associated,
    )
    .await;
    Ok(UiTerminalInputSubmissionResult {
        context: acknowledgement,
        history,
    })
}

/// 调用方已确认启用历史；只有 writer gate 关联同一 binding/context 且写入 ACK 后才触碰仓储，历史故障不回滚输入。
///
/// @author kongweiguang
async fn record_ui_terminal_submission_history_after_ack(
    commands: &CommandsService,
    input: UiTerminalInputRequest,
    command: String,
    before: TerminalCommandContext,
    session: SessionInfo,
    acknowledgement: TerminalCommandContext,
    command_associated: bool,
) -> TerminalInputSubmissionReceipt {
    let Some(expected) = input.expected_context else {
        return TerminalInputSubmissionReceipt::default();
    };
    if !command_associated
        || !is_ui_history_candidate(&input, &command, true)
        || !is_ui_history_context(&before, &input.binding)
        || expected != before.version()
        || !ui_submission_is_proven(&input, &before, &acknowledgement, &session, expected)
    {
        return TerminalInputSubmissionReceipt::default();
    }

    receipt_after_record(
        commands
            .record_history(history_request(
                command,
                CommandHistorySource::User,
                &session,
            ))
            .await,
    )
}

/// 只接受有明确 tracked Enter 与安全正文的候选；shell/context/ACK 仍需后续独立证明。
///
/// @author kongweiguang
fn is_ui_history_candidate(
    input: &UiTerminalInputRequest,
    command: &str,
    record_history: bool,
) -> bool {
    record_history
        && input.expected_context.is_some()
        && input.projection == UiInputProjection::Tracked
        && is_prompt_submit_event(&input.event)
        && is_history_text(command)
}

/// MCP 写入不按历史资格拒绝原文；writer ack 缺 admission 或内容不合约时只跳过历史。
async fn write_mcp_terminal_input(
    commands: &CommandsService,
    session_id: SessionId,
    data: String,
) -> AppResult<TerminalInputSubmissionReceipt> {
    let command = single_printable_mcp_command(&data).map(str::to_owned);
    let payload_bytes = data.len();
    let session = if command.is_some() {
        // 写前读取目标元数据用于组装历史行，资格仍只由随后返回的 writer admission 证明。
        commands.session_info(session_id.clone()).await.ok()
    } else {
        None
    };

    // 准入前态、输入 generation 与完整 writer ack 来自一次原子回执，避免快 prompt 竞态。
    let write_receipt = commands
        .sessions
        .write_with_receipt(session_id.clone(), data.into_bytes())
        .await?;
    let Some(command) = command else {
        return Ok(TerminalInputSubmissionReceipt::default());
    };
    if !mcp_history_admission_is_proven(
        write_receipt.admission.as_ref(),
        &session_id,
        write_receipt.acknowledged_bytes,
        payload_bytes,
    ) {
        return Ok(TerminalInputSubmissionReceipt::default());
    }
    let Some(admission) = write_receipt.admission.as_ref() else {
        return Ok(TerminalInputSubmissionReceipt::default());
    };
    let Some(session) = session.as_ref() else {
        return Ok(TerminalInputSubmissionReceipt::default());
    };
    if !history_session_matches_admission(session, admission) {
        return Ok(TerminalInputSubmissionReceipt::default());
    }

    Ok(receipt_after_record(
        commands
            .record_history(history_request(
                command,
                CommandHistorySource::Tool,
                session,
            ))
            .await,
    ))
}

/// UI ack 的 prompt epoch 允许前进：快命令可在 write ack 取回前发起下一 prompt。
fn ui_submission_is_proven(
    input: &UiTerminalInputRequest,
    before: &TerminalCommandContext,
    acknowledgement: &TerminalCommandContext,
    session: &SessionInfo,
    expected: TerminalCommandContextVersion,
) -> bool {
    let next_input_generation = expected.input_generation.checked_add(1);
    input.expected_context == Some(expected)
        && input.projection == UiInputProjection::Tracked
        && is_prompt_submit_event(&input.event)
        && is_ui_history_context(before, &input.binding)
        && expected == before.version()
        && session.lifecycle == SessionLifecycle::Running
        && session.session_id == input.binding.session_id
        && session.owner == input.binding.owner
        && acknowledgement.session_id == before.session_id
        && acknowledgement.owner == before.owner
        && acknowledgement.session_generation == expected.session_generation
        && acknowledgement.prompt_epoch >= expected.prompt_epoch
        && next_input_generation == Some(acknowledgement.input_generation)
}

/// 只对 UI 所属、可信且当前 prompt 的 typed projection 捕获 User 来源。
fn is_ui_history_context(context: &TerminalCommandContext, binding: &UiSessionBinding) -> bool {
    matches!(&binding.owner, SessionOwner::Ui { .. })
        && context.session_id == binding.session_id
        && context.owner == binding.owner
        && context.session_generation == binding.session_generation
        && context.availability == TerminalCommandAvailability::Available
        && context.prompt_ready
        && !context.alternate_screen
        && context.shell_dialect != RemoteShellDialect::Unknown
        && !context.prompt_line_empty
}

/// MCP 历史资格只读取同一次完整 writer ACK 的前态和 generation，不用事后快照猜测归属。
fn mcp_history_admission_is_proven(
    admission: Option<&TerminalWriteAdmission>,
    expected_session_id: &SessionId,
    acknowledged_bytes: usize,
    payload_bytes: usize,
) -> bool {
    let Some(admission) = admission else {
        return false;
    };
    let next_generation = admission.input_generation_before.checked_add(1);
    acknowledged_bytes == payload_bytes
        && payload_bytes > 0
        && &admission.session_id == expected_session_id
        && admission.lifecycle == SessionLifecycle::Running
        && admission.shell_dialect != RemoteShellDialect::Unknown
        && admission.shell_integration_enabled
        && admission.shell_integration_observed
        && admission.prompt_ready
        && admission.prompt_line_empty
        && !admission.alternate_screen
        && next_generation == Some(admission.accepted_input_generation)
}

/// admission 决定是否记录；该输入前快照只提供目标和 cwd，且必须属于同一运行会话。
fn history_session_matches_admission(
    session: &SessionInfo,
    admission: &TerminalWriteAdmission,
) -> bool {
    session.session_id == admission.session_id
        && session.owner == admission.owner
        && session.lifecycle == SessionLifecycle::Running
        && admission.lifecycle == SessionLifecycle::Running
}

/// 仅剥离唯一结尾 CR/LF，并拒绝正文中的换行、ANSI、NUL 和其它控制字符。
fn single_printable_mcp_command(data: &str) -> Option<&str> {
    let command = data
        .strip_suffix("\r\n")
        .or_else(|| data.strip_suffix('\r'))
        .or_else(|| data.strip_suffix('\n'))?;
    if command.trim().is_empty()
        || command.chars().any(|character| character.is_control())
        || command.len() > 32_000
        || command.chars().count() > 4_000
    {
        return None;
    }
    Some(command)
}

/// 不记录未规范化的 shell 控制序列；LF 只允许在 UI projection 的多行草稿中出现。
fn is_history_text(command: &str) -> bool {
    !command.trim().is_empty()
        && !command
            .chars()
            .any(|character| character.is_control() && character != '\n')
        && NormalizedCommandHistory::new(command).is_ok()
}

/// 只把无修饰的 Enter press 当作整行提交；Kitty 模式能区分组合键，Alt 还会编码 ESC 前缀。
fn is_prompt_submit_event(event: &TerminalInputEvent) -> bool {
    let is_enter = |key: &kerminal_terminal::input::TerminalKeyEvent| {
        key.key == TerminalKey::Enter
            && key.phase == KeyPhase::Press
            && key.modifiers == Default::default()
    };
    match event {
        TerminalInputEvent::Key(key) | TerminalInputEvent::KeyWithText { event: key, .. } => {
            is_enter(key)
        }
        TerminalInputEvent::Text(_)
        | TerminalInputEvent::Paste(_)
        | TerminalInputEvent::Mouse(_)
        | TerminalInputEvent::Focus(_) => false,
    }
}

/// 从已绑定会话构造单条历史请求，来源和目标由 application 决定而非外部参数决定。
fn history_request(
    command: String,
    source: CommandHistorySource,
    session: &SessionInfo,
) -> CommandHistoryRecordRequest {
    CommandHistoryRecordRequest {
        command,
        source,
        target: execution::history_target(&session.target),
        record: Some(true),
        session_id: Some(session.session_id.clone()),
        pane_id: execution::owner_pane(&session.owner),
        tab_id: execution::owner_tab(&session.owner),
        profile_id: None,
        remote_host_id: execution::target_host(&session.target),
        cwd: execution::target_cwd(&session.target),
        shell: execution::local_shell(&session.target),
    }
}

/// 历史仓储失败是已写入终端后的辅助结果，不得让调用方把整笔操作判为未发送。
fn receipt_after_record(
    result: AppResult<CommandHistoryRecordResult>,
) -> TerminalInputSubmissionReceipt {
    match result {
        Ok(history_record) => TerminalInputSubmissionReceipt {
            history_record: Some(history_record),
            history_error_code: None,
        },
        Err(error) => TerminalInputSubmissionReceipt {
            history_record: None,
            history_error_code: Some(error.public_code()),
        },
    }
}

impl Default for TerminalInputSubmissionReceipt {
    /// 未达到记录边界时只表示 history 未尝试；外层 Ok 已独立证明输入写入成功。
    fn default() -> Self {
        Self {
            history_record: None,
            history_error_code: None,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/command_submission.rs"]
mod tests;
