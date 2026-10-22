//! 命令片段执行用例：精确绑定目录版本、可见终端和一次性确认。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, fmt, sync::Arc, time::Duration};

use kerminal_domain::{
    commands::{
        CommandHistoryRecordRequest, CommandHistoryRecordResult, CommandHistorySource,
        SnippetApplicabilityStatus, SnippetDefinition, SnippetPreferenceKey, SnippetRisk,
        SnippetUsageAction, SnippetUsageReceipt, SnippetUsageRecordRequest,
        assess_snippet_applicability,
    },
    preferences::ConfigRevision,
    sessions::{SessionInfo, SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{
    AppError, AppResult, PublicErrorCode,
    commands::execution::{
        history_target, local_shell, owner_pane, owner_tab, target_cwd, target_host,
    },
    commands::{
        CommandsService, SnippetCatalogRevision, SnippetExecutionCatalogSnapshot,
        SnippetRenderDialect,
    },
    sessions::{
        SessionFilter, SessionService, ShellCommandOutcome, ShellCommandRequest,
        TerminalCommandAvailability, TerminalCommandContext, TerminalCommandContextVersion,
        TerminalCommandUnavailableReason, TerminalInputEvent, UiInputProjection, UiSessionBinding,
        UiTerminalInputRequest,
    },
};

/// 片段运行沿用工作流的长命令预算，避免 UI 超时后丢弃仍由终端跟踪的命令 future。
const SNIPPET_RUN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// 运行目标的安全类型摘要；confirmation 不携带 cwd、环境变量、远端命令或凭据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnippetRunTargetSummary {
    Local,
    Ssh,
    Container,
    Telnet,
    Serial,
}

/// 供 UI 展示执行前确认的信息；敏感变量只显示原模板，不包含展开后的值。
#[derive(Clone)]
pub struct SnippetRunConfirmationInfo {
    pub key: SnippetPreferenceKey,
    pub title: String,
    /// 创建预览时的终端身份；展示时不应改用当前选中的 Tab 推断目标。
    pub session_id: kerminal_domain::identity::SessionId,
    /// 创建预览时的 UI owner，供 UI 将确认说明与原 Pane/Tab 对齐。
    pub owner: SessionOwner,
    pub risk: SnippetRisk,
    pub sensitive: bool,
    pub requires_confirmation: bool,
    pub target: SnippetRunTargetSummary,
    pub command_preview: String,
}

impl fmt::Debug for SnippetRunConfirmationInfo {
    /// 调试输出不回显命令预览，避免模板或用户输入意外进入日志。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnippetRunConfirmationInfo")
            .field("key", &self.key)
            .field("title", &self.title)
            .field("session_id", &self.session_id)
            .field("owner", &self.owner)
            .field("risk", &self.risk)
            .field("sensitive", &self.sensitive)
            .field("requires_confirmation", &self.requires_confirmation)
            .field("target", &self.target)
            .field("command_preview_bytes", &self.command_preview.len())
            .finish()
    }
}

/// UI 发起一次运行的请求；变量值只进入内存中的 plan，不进入调试输出或用量记录。
pub struct SnippetRunPreparation {
    pub key: SnippetPreferenceKey,
    pub binding: UiSessionBinding,
    pub variables: BTreeMap<String, String>,
    /// 对应用户现有的命令历史偏好；敏感片段仍会在用例内强制跳过。
    pub record_history: bool,
}

impl fmt::Debug for SnippetRunPreparation {
    /// 不展开变量值，尤其不输出 Secret 类型输入。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnippetRunPreparation")
            .field("key", &self.key)
            .field("binding", &self.binding)
            .field("variable_names", &self.variables.keys().collect::<Vec<_>>())
            .field("record_history", &self.record_history)
            .finish()
    }
}

/// 一次性运行计划；字段私有，调用方不能替换模板、目标、generation 或方言。
pub struct SnippetRunPlan {
    plan_id: Uuid,
    snapshot: SnippetExecutionCatalogSnapshot,
    binding: UiSessionBinding,
    target: SessionTarget,
    dialect: SnippetRenderDialect,
    variables: BTreeMap<String, String>,
    command: String,
    record_history: bool,
    confirmation: SnippetRunConfirmationInfo,
}

impl fmt::Debug for SnippetRunPlan {
    /// plan 的 Debug 只暴露稳定身份和安全元数据，不回显定义正文、变量或渲染命令。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnippetRunPlan")
            .field("key", &self.snapshot.key)
            .field("revision", &self.snapshot.revision)
            .field("binding", &self.binding)
            .field("risk", &self.confirmation.risk)
            .field("sensitive", &self.confirmation.sensitive)
            .field("command_bytes", &self.command.len())
            .field("record_history", &self.record_history)
            .finish_non_exhaustive()
    }
}

impl Drop for SnippetRunPlan {
    /// plan 结束时尽可能清除变量、展开命令和预览副本，避免延长敏感值的内存驻留。
    fn drop(&mut self) {
        for value in self.variables.values_mut() {
            value.zeroize();
        }
        self.command.zeroize();
        self.snapshot.definition.command.zeroize();
        self.confirmation.command_preview.zeroize();
    }
}

impl SnippetRunPlan {
    /// 返回创建 plan 时生成的展示快照，内容不会因后续目录或 Tab 变化而改绑。
    pub fn confirmation(&self) -> &SnippetRunConfirmationInfo {
        &self.confirmation
    }

    /// 仅在 UI 明确确认当前展示信息后调用；令牌绑定到此 plan，不能用于其它执行计划。
    pub fn confirm(&self) -> SnippetRunConfirmation {
        SnippetRunConfirmation {
            plan_id: self.plan_id,
        }
    }
}

/// 填入计划只保存精确目录与提示符版本；不提供执行确认或隐式发送回车的入口。
#[derive(Debug)]
pub struct SnippetFillPlan {
    plan: SnippetRunPlan,
    context: TerminalCommandContextVersion,
}

/// 绑定到单个 plan 的显式确认令牌，禁止用通用布尔值确认另一个目录项或目标。
pub struct SnippetRunConfirmation {
    plan_id: Uuid,
}

impl fmt::Debug for SnippetRunConfirmation {
    /// 不公开 plan nonce，调试日志只说明存在类型化确认凭证。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnippetRunConfirmation([redacted])")
    }
}

/// 只有真实 OSC 133 Finished 才有 usage 结果；记录失败单独呈现，不把已执行命令改报失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnippetRunUsage {
    Recorded(SnippetUsageReceipt),
    NotRecorded { error_code: PublicErrorCode },
}

/// 命令历史结果独立于 shell 完成状态；策略跳过和存储失败不会把已执行命令改报失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnippetRunHistory {
    NotRequested,
    SkippedSensitive,
    Evaluated(Arc<CommandHistoryRecordResult>),
    Failed { error_code: PublicErrorCode },
}

/// 已确认完成的命令可有非零退出码；Unknown 不触发 usage，也不提供隐式重试语义。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnippetRunResult {
    Finished {
        command_id: String,
        exit_code: i32,
        usage: SnippetRunUsage,
        history: SnippetRunHistory,
    },
    Unknown {
        command_id: String,
        reason: crate::sessions::ShellCommandUnknownReason,
    },
}

/// 片段运行用例复用 Commands 的目录/转义/usage ports 和同一 SessionService 的执行边界。
pub struct SnippetExecutionService {
    commands: Arc<CommandsService>,
    sessions: Arc<dyn SessionService>,
}

impl SnippetExecutionService {
    /// 组合根传入共享应用服务，避免片段执行另建目录、终端或用量存储实例。
    pub fn new(commands: Arc<CommandsService>, sessions: Arc<dyn SessionService>) -> Self {
        Self { commands, sessions }
    }

    /// 将当前精确目录项、UI binding、generation 与可信 shell 方言冻结成一次运行计划。
    pub async fn prepare_run(&self, request: SnippetRunPreparation) -> AppResult<SnippetRunPlan> {
        if !matches!(&request.binding.owner, SessionOwner::Ui { .. }) {
            return Err(AppError::conflict("命令片段只能运行在明确绑定的可见终端中"));
        }
        let snapshot = self
            .commands
            .get_snippet_execution_snapshot(request.key.clone())
            .await?
            .ok_or_else(|| AppError::not_found("命令片段目录项不存在"))?;
        if snapshot.key != request.key {
            return Err(AppError::conflict("命令片段目录身份与请求不匹配"));
        }

        let (session, context, dialect) = self
            .resolve_ui_binding(&request.binding, None, None)
            .await?;
        ensure_snippet_applicability(&snapshot, Some(context.shell_dialect))?;
        let rendered = self
            .commands
            .render_snippet_for_binding(&snapshot.definition, &request.binding, &request.variables)
            .await?;
        let sensitive = rendered.sensitive;
        let risk = snapshot.definition.risk;
        let requires_confirmation = requires_confirmation(risk, sensitive);
        let preview = if sensitive {
            snapshot.definition.command.clone()
        } else {
            rendered.command.clone()
        };
        let confirmation = SnippetRunConfirmationInfo {
            key: snapshot.key.clone(),
            title: snapshot.definition.title.clone(),
            session_id: session.session_id.clone(),
            owner: session.owner.clone(),
            risk,
            sensitive,
            requires_confirmation,
            target: target_summary(&session.target),
            command_preview: preview,
        };
        let SnippetRunPreparation {
            key: _,
            binding,
            variables,
            record_history,
        } = request;
        let plan = SnippetRunPlan {
            plan_id: Uuid::new_v4(),
            snapshot,
            binding,
            target: session.target,
            dialect,
            variables,
            command: rendered.command,
            record_history,
            confirmation,
        };
        Ok(plan)
    }

    /// 填入沿用可信模板解析，但必须与 UI 已展示的定义和版本一致，且只接受不会提交命令的单行文本。
    /// @author kongweiguang
    pub async fn prepare_fill(
        &self,
        request: SnippetRunPreparation,
        expected_definition: SnippetDefinition,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<SnippetFillPlan> {
        let plan = self.prepare_run(request).await?;
        let revision_matches = match (&plan.snapshot.revision, expected_revision) {
            (SnippetCatalogRevision::User(actual), Some(expected)) => *actual == expected,
            (SnippetCatalogRevision::Builtin { .. }, None) => true,
            _ => false,
        };
        if plan.snapshot.definition != expected_definition || !revision_matches {
            return Err(AppError::conflict(
                "命令片段在填入前已变化，请重新选择后核对",
            ));
        }
        if plan.command.is_empty() || plan.command.chars().any(char::is_control) {
            return Err(AppError::invalid(
                "填入只支持单行命令；多行内容请通过运行预览执行",
            ));
        }
        let (_, context, _) = self
            .resolve_ui_binding(&plan.binding, Some(&plan.target), Some(plan.dialect))
            .await?;
        Ok(SnippetFillPlan {
            plan,
            context: context.version(),
        })
    }

    /// 写入前再次核对目录、目标、方言及提示符版本；底层 writer 原子核对 context，只发送 Paste，不重放未知结果。
    /// @author kongweiguang
    pub async fn fill(
        &self,
        fill: SnippetFillPlan,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalCommandContext> {
        let plan = &fill.plan;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("片段填入已取消"));
        }
        let current = self
            .commands
            .get_snippet_execution_snapshot(plan.snapshot.key.clone())
            .await?
            .ok_or_else(|| AppError::not_found("命令片段已不存在"))?;
        if !same_catalog_snapshot(&plan.snapshot, &current) {
            return Err(AppError::conflict("命令片段在填入期间已变化，请重新核对"));
        }
        let (_, context, _) = self
            .resolve_ui_binding(&plan.binding, Some(&plan.target), Some(plan.dialect))
            .await?;
        if context.version() != fill.context {
            return Err(AppError::conflict(
                "终端输入在填入期间已变化，请核对终端后重新填入",
            ));
        }
        let mut rendered = self
            .commands
            .render_snippet_for_binding(&plan.snapshot.definition, &plan.binding, &plan.variables)
            .await?;
        let matches =
            rendered.command == plan.command && rendered.sensitive == plan.confirmation.sensitive;
        rendered.command.zeroize();
        if !matches {
            return Err(AppError::conflict("命令片段解析结果已变化，请重新核对"));
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("片段填入已取消"));
        }
        self.sessions
            .write_ui_input(UiTerminalInputRequest {
                binding: plan.binding.clone(),
                expected_context: Some(fill.context),
                projection: UiInputProjection::Tracked,
                event: TerminalInputEvent::Paste(plan.command.clone()),
            })
            .await
    }

    /// 运行前重新核对目录 revision、目标、generation、方言与 plan-bound 确认，再提交一次受管 shell 命令。
    pub async fn run(
        &self,
        plan: SnippetRunPlan,
        confirmation: Option<SnippetRunConfirmation>,
        cancellation: CancellationToken,
    ) -> AppResult<SnippetRunResult> {
        validate_confirmation(&plan, confirmation.as_ref())?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令片段运行已取消"));
        }

        let current = self
            .commands
            .get_snippet_execution_snapshot(plan.snapshot.key.clone())
            .await?
            .ok_or_else(|| AppError::not_found("命令片段已不存在"))?;
        if !same_catalog_snapshot(&plan.snapshot, &current) {
            return Err(AppError::conflict("命令片段在确认后已变化，请重新预览"));
        }

        let (_, before_context, current_dialect) = self
            .resolve_ui_binding(&plan.binding, Some(&plan.target), Some(plan.dialect))
            .await?;
        ensure_snippet_applicability(&plan.snapshot, Some(before_context.shell_dialect))?;
        let mut rendered = self
            .commands
            .render_snippet_for_binding(&plan.snapshot.definition, &plan.binding, &plan.variables)
            .await?;
        let rendered_matches = rendered.sensitive == plan.confirmation.sensitive
            && rendered.command == plan.command
            && current_dialect == plan.dialect;
        rendered.command.zeroize();
        if !rendered_matches {
            return Err(AppError::conflict(
                "命令片段目标或 shell 方言已变化，请重新预览",
            ));
        }
        let (session, after_context, after_dialect) = self
            .resolve_ui_binding(&plan.binding, Some(&plan.target), Some(plan.dialect))
            .await?;
        if before_context != after_context || after_dialect != plan.dialect {
            return Err(AppError::conflict("命令片段目标状态在提交前已变化"));
        }
        ensure_snippet_applicability(&plan.snapshot, Some(after_context.shell_dialect))?;

        let command_id = Uuid::new_v4().to_string();
        let request = ShellCommandRequest {
            session_id: plan.binding.session_id.clone(),
            expected_ui_binding: Some(plan.binding.clone()),
            command_id: command_id.clone(),
            command: plan.command.clone(),
            timeout: SNIPPET_RUN_TIMEOUT,
        };
        // 不能在外层 select 中 drop 执行 future；SessionService 负责把取消映射成可证明结果。
        let outcome = self
            .sessions
            .execute_shell_command(request, cancellation)
            .await?;
        match outcome {
            ShellCommandOutcome::Finished {
                command_id: reported_command_id,
                exit_code,
            } => {
                if reported_command_id != command_id {
                    return Err(AppError::outcome_unknown(
                        "终端命令回执与本次片段请求不匹配",
                    ));
                }
                let history = self.record_finished_history(&plan, &session).await;
                let usage = match self
                    .commands
                    .record_snippet_usage(SnippetUsageRecordRequest {
                        receipt_id: command_id.clone(),
                        key: plan.snapshot.key.clone(),
                        action: SnippetUsageAction::Run,
                    })
                    .await
                {
                    Ok(receipt) => SnippetRunUsage::Recorded(receipt),
                    Err(error) => SnippetRunUsage::NotRecorded {
                        error_code: error.public_code(),
                    },
                };
                Ok(SnippetRunResult::Finished {
                    command_id,
                    exit_code,
                    usage,
                    history,
                })
            }
            ShellCommandOutcome::Unknown {
                command_id: reported_command_id,
                reason,
            } => {
                if reported_command_id != command_id {
                    return Err(AppError::outcome_unknown(
                        "终端未知结果与本次片段请求不匹配",
                    ));
                }
                Ok(SnippetRunResult::Unknown { command_id, reason })
            }
        }
    }

    /// 仅在命令回执匹配且已 Finished 后写普通历史；敏感命令正文绝不传给仓储。
    /// @author kongweiguang
    async fn record_finished_history(
        &self,
        plan: &SnippetRunPlan,
        session: &SessionInfo,
    ) -> SnippetRunHistory {
        if !plan.record_history {
            return SnippetRunHistory::NotRequested;
        }
        if plan.confirmation.sensitive {
            return SnippetRunHistory::SkippedSensitive;
        }
        match self
            .commands
            .record_history(CommandHistoryRecordRequest {
                command: plan.command.clone(),
                source: CommandHistorySource::Snippet,
                target: history_target(&session.target),
                record: Some(true),
                session_id: Some(session.session_id.clone()),
                pane_id: owner_pane(&session.owner),
                tab_id: owner_tab(&session.owner),
                profile_id: None,
                remote_host_id: target_host(&session.target),
                cwd: target_cwd(&session.target),
                shell: local_shell(&session.target),
            })
            .await
        {
            Ok(result) => SnippetRunHistory::Evaluated(Arc::new(result)),
            Err(error) => SnippetRunHistory::Failed {
                error_code: error.public_code(),
            },
        }
    }

    /// 分别核实 UI owner、会话代次和提示符可用性，避免把未知输入状态误报为目标漂移。
    /// 对无法证明的终端输入保持 fail-closed，并提供与具体不可用原因相符的恢复提示。
    ///
    /// @author kongweiguang
    async fn resolve_ui_binding(
        &self,
        binding: &UiSessionBinding,
        expected_target: Option<&SessionTarget>,
        expected_dialect: Option<SnippetRenderDialect>,
    ) -> AppResult<(
        SessionInfo,
        crate::sessions::TerminalCommandContext,
        SnippetRenderDialect,
    )> {
        if !matches!(&binding.owner, SessionOwner::Ui { .. }) {
            return Err(AppError::conflict("命令片段目标不属于可见终端"));
        }
        let session = self
            .sessions
            .list(SessionFilter::default())
            .await?
            .into_iter()
            .find(|session| session.session_id == binding.session_id)
            .ok_or_else(|| AppError::not_found("指定终端会话不存在"))?;
        if session.lifecycle != SessionLifecycle::Running
            || session.owner != binding.owner
            || expected_target.is_some_and(|target| target != &session.target)
        {
            return Err(AppError::conflict("命令片段目标会话已变化或已关闭"));
        }
        let context = self
            .sessions
            .terminal_command_context(binding.session_id.clone())
            .await?;
        if context.session_id != binding.session_id
            || context.owner != binding.owner
            || context.session_generation != binding.session_generation
        {
            return Err(AppError::conflict("命令片段目标绑定或会话代次已变化"));
        }
        if let TerminalCommandAvailability::Unavailable(reason) = context.availability {
            let message = match reason {
                TerminalCommandUnavailableReason::NonUiOwner => "命令片段目标不属于可见终端",
                TerminalCommandUnavailableReason::SessionNotRunning => "命令片段目标会话当前未运行",
                TerminalCommandUnavailableReason::ShellIntegrationDisabled => {
                    "终端 Shell 集成未启用，无法安全确认命令提示符"
                }
                TerminalCommandUnavailableReason::ShellIntegrationNotObserved => {
                    "尚未确认终端 Shell 集成状态，请等待提示符状态可用"
                }
                TerminalCommandUnavailableReason::PromptNotReady => {
                    "终端当前没有可确认的空闲命令提示符，请等待提示符就绪后重试"
                }
                TerminalCommandUnavailableReason::AlternateScreen => {
                    "终端处于备用屏幕，无法确认命令提示符"
                }
                TerminalCommandUnavailableReason::UnsupportedShellDialect => {
                    "当前 Shell 方言无法安全确认命令提示符"
                }
                TerminalCommandUnavailableReason::ExternalInputObserved => {
                    "检测到外部终端输入，命令行状态无法确认"
                }
                TerminalCommandUnavailableReason::InputWritePending => {
                    "终端输入尚未完成回执，无法确认命令行状态"
                }
                TerminalCommandUnavailableReason::UiInputProjectionInvalidated => {
                    "终端输入状态暂不可核对，请回到新的命令提示符后重试"
                }
                TerminalCommandUnavailableReason::ManagedCommandActive => {
                    "终端正在执行其他受管命令，请等待结束后重试"
                }
            };
            return Err(AppError::conflict(message));
        }
        let dialect = SnippetRenderDialect::from_trusted_shell(context.shell_dialect)?;
        if expected_dialect.is_some_and(|expected| expected != dialect) {
            return Err(AppError::conflict("命令片段 shell 方言已变化"));
        }
        Ok((session, context, dialect))
    }
}

/// 只有只读、非敏感片段免确认；任何修改风险或秘密输入都要求 plan-bound 明确确认。
fn requires_confirmation(risk: SnippetRisk, sensitive: bool) -> bool {
    sensitive || risk != SnippetRisk::Inspect
}

/// 比较快照的来源、revision 和完整定义，避免依赖列表顺序、标题或模板局部相等。
fn same_catalog_snapshot(
    expected: &SnippetExecutionCatalogSnapshot,
    actual: &SnippetExecutionCatalogSnapshot,
) -> bool {
    expected.key == actual.key
        && expected.revision == actual.revision
        && expected.definition == actual.definition
        && expected.platforms == actual.platforms
        && expected.shells == actual.shells
}

/// 在准备和提交边界复用同一领域规则，只阻止有可信证据的内置 shell 家族冲突。
fn ensure_snippet_applicability(
    snapshot: &SnippetExecutionCatalogSnapshot,
    shell_dialect: Option<kerminal_domain::connections::RemoteShellDialect>,
) -> AppResult<()> {
    let assessment = assess_snippet_applicability(
        snapshot.key.origin,
        &snapshot.platforms,
        &snapshot.shells,
        shell_dialect,
    );
    if assessment.status == SnippetApplicabilityStatus::Incompatible {
        return Err(AppError::capability_not_supported(
            "当前 shell 与此内置片段声明的 shell 范围不兼容",
        ));
    }
    Ok(())
}

/// 把目标映射成不含路径、环境或远端命令的安全摘要。
fn target_summary(target: &SessionTarget) -> SnippetRunTargetSummary {
    match target {
        SessionTarget::Local(_) => SnippetRunTargetSummary::Local,
        SessionTarget::Ssh(_) => SnippetRunTargetSummary::Ssh,
        SessionTarget::Container(_) => SnippetRunTargetSummary::Container,
        SessionTarget::Telnet(_) => SnippetRunTargetSummary::Telnet,
        SessionTarget::Serial(_) => SnippetRunTargetSummary::Serial,
    }
}

/// 校验确认令牌与执行计划完全对应，未确认或跨 plan 复用均在终端边界前拒绝。
fn validate_confirmation(
    plan: &SnippetRunPlan,
    confirmation: Option<&SnippetRunConfirmation>,
) -> AppResult<()> {
    match (plan.confirmation.requires_confirmation, confirmation) {
        (true, None) => Err(AppError::conflict("运行此命令片段需要明确确认")),
        (true, Some(token)) | (false, Some(token)) if token.plan_id != plan.plan_id => {
            Err(AppError::conflict("命令片段确认不属于当前运行计划"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/snippet_execution.rs"]
mod tests;
