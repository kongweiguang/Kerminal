//! 命令片段的显式运行协调；运行计划与终端目标在一次点击中冻结。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, sync::Arc};

use gpui_kit::{Context, Window};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_application::{
    AppError, ApplicationTaskExecutor, PublicErrorCode,
    commands::{
        SnippetExecutionService, SnippetRunConfirmation, SnippetRunConfirmationInfo,
        SnippetRunPlan, SnippetRunPreparation, SnippetRunResult,
    },
};
use kerminal_domain::{
    commands::{SnippetCatalogOrigin, SnippetDefinition, SnippetPreferenceKey},
    identity::SessionId,
    preferences::ConfigRevision,
};
use tokio_util::sync::CancellationToken;

use super::{CommandsView, SnippetDraftValues};

/// 运行阶段把“尚未提交”“正在提交”和“结果未知”分开，防止 UI 把未知执行误当取消。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SnippetRunPhase {
    #[default]
    Idle,
    Preparing,
    AwaitingConfirmation,
    Running,
    CancellationRequested,
    Finished,
    OutcomeUnknown,
    OutcomeReviewed,
    Cancelled,
    Failed,
}

/// 唯一持有 plan、捕获目标和取消令牌的 UI 状态；关闭弹层只影响可见性。
pub(super) struct SnippetRunUiState {
    phase: SnippetRunPhase,
    generation: u64,
    modal_open: bool,
    pending_plan: Option<SnippetRunPlan>,
    confirmation: Option<SnippetRunConfirmationInfo>,
    preview_identity: Option<SnippetPreviewIdentity>,
    result: Option<SnippetRunResult>,
    status: Option<String>,
    error: Option<String>,
    cancellation: Option<CancellationToken>,
    unknown_session_id: Option<SessionId>,
}

impl Default for SnippetRunUiState {
    /// 新面板不自动运行片段，也不预设任何确认或目标状态。
    fn default() -> Self {
        Self {
            phase: SnippetRunPhase::Idle,
            generation: 0,
            modal_open: false,
            pending_plan: None,
            confirmation: None,
            preview_identity: None,
            result: None,
            status: None,
            error: None,
            cancellation: None,
            unknown_session_id: None,
        }
    }
}

impl SnippetRunUiState {
    /// 返回当前阶段的值快照，renderer 不可直接改写运行状态。
    pub(super) fn phase(&self) -> SnippetRunPhase {
        self.phase
    }

    /// 返回 plan 创建时的目标和安全预览，不读取后来选中的 Tab。
    pub(super) fn confirmation(&self) -> Option<&SnippetRunConfirmationInfo> {
        self.confirmation.as_ref()
    }

    /// 只把仍待确认的 plan 投影回同一片段 revision 与完整终端 binding，避免旧目标预览冒充当前内容。
    ///
    /// @author kongweiguang
    pub(super) fn inline_preview(
        &self,
        key: &SnippetPreferenceKey,
        revision: Option<&ConfigRevision>,
        binding: &UiSessionBinding,
    ) -> Option<&SnippetRunConfirmationInfo> {
        if self.phase != SnippetRunPhase::AwaitingConfirmation || self.pending_plan.is_none() {
            return None;
        }
        let identity = self.preview_identity.as_ref()?;
        if !identity.matches(key, revision, binding) {
            return None;
        }
        self.confirmation.as_ref().filter(|info| {
            &info.key == key && info.session_id == binding.session_id && info.owner == binding.owner
        })
    }

    /// 返回完成或未知的 typed receipt，UI 可分别显示退出码及 usage/history 记录结果。
    pub(super) fn result(&self) -> Option<&SnippetRunResult> {
        self.result.as_ref()
    }

    /// 返回不包含模板变量值的阶段说明。
    pub(super) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// 返回应用层安全错误文案，不暴露底层命令、路径或 source。
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 只有准备、提交或等待协作取消的任务才算忙；待确认计划可明确确认或取消。
    pub(super) fn is_busy(&self) -> bool {
        matches!(
            self.phase,
            SnippetRunPhase::Preparing
                | SnippetRunPhase::Running
                | SnippetRunPhase::CancellationRequested
        )
    }

    /// 准备、确认和提交期间锁住表单，避免用户看到的变量与捕获 plan 不一致。
    pub(super) fn blocks_editing(&self) -> bool {
        matches!(
            self.phase,
            SnippetRunPhase::Preparing
                | SnippetRunPhase::AwaitingConfirmation
                | SnippetRunPhase::Running
                | SnippetRunPhase::CancellationRequested
        )
    }

    /// 只有终态或显式核对后的记录可开始下一次运行；未知结果必须先由用户查看原终端。
    pub(super) fn can_start(&self) -> bool {
        matches!(
            self.phase,
            SnippetRunPhase::Idle
                | SnippetRunPhase::Finished
                | SnippetRunPhase::OutcomeReviewed
                | SnippetRunPhase::Cancelled
                | SnippetRunPhase::Failed
        )
    }

    /// 运行尚待确认、提交或人工核对时保留原片段恢复入口，不能被切换目标隐藏。
    ///
    /// @author kongweiguang
    pub(super) fn blocks_navigation(&self) -> bool {
        matches!(
            self.phase,
            SnippetRunPhase::Preparing
                | SnippetRunPhase::AwaitingConfirmation
                | SnippetRunPhase::Running
                | SnippetRunPhase::CancellationRequested
                | SnippetRunPhase::OutcomeUnknown
        )
    }

    /// 返回弹层可见状态；隐藏弹层不会取消或释放正在执行的 plan。
    pub(super) fn is_open(&self) -> bool {
        self.modal_open
    }

    /// 固定恢复区在运行面板打开或运行仍需处理时保持可见，终态结果由用户显式收起。
    ///
    /// @author kongweiguang
    pub(super) fn recovery_visible(&self) -> bool {
        self.modal_open || self.blocks_navigation()
    }

    /// 返回发生未知结果时的原 session，供 UI 提示用户检查同一终端。
    pub(super) fn unknown_session_id(&self) -> Option<&SessionId> {
        self.unknown_session_id.as_ref()
    }

    /// 开始一轮全新准备并推进代次，清理旧预览身份以阻止迟到 plan 泄漏到新运行。
    ///
    /// @author kongweiguang
    fn begin_preparation(&mut self) -> Option<(u64, CancellationToken)> {
        if !self.can_start() {
            return None;
        }
        self.generation = self.generation.wrapping_add(1).max(1);
        self.phase = SnippetRunPhase::Preparing;
        self.modal_open = true;
        self.pending_plan = None;
        self.confirmation = None;
        self.preview_identity = None;
        self.result = None;
        self.status =
            Some(crate::locale::text("正在准备运行预览…", "Preparing run preview…").to_string());
        self.error = None;
        self.unknown_session_id = None;
        let cancellation = CancellationToken::new();
        self.cancellation = Some(cancellation.clone());
        Some((self.generation, cancellation))
    }

    /// 捕获的 identity 与异步 prepare request 共用值快照，后续目标或 revision 变化时隐藏旧预览。
    ///
    /// @author kongweiguang
    pub(super) fn begin_preparation_with_preview(
        &mut self,
        identity: SnippetPreviewIdentity,
    ) -> Option<(u64, CancellationToken)> {
        let request = self.begin_preparation()?;
        self.preview_identity = Some(identity);
        Some(request)
    }

    /// 记住需要显式确认的 plan；目录或当前 Tab 后续变化不会改写该快照。
    fn await_confirmation(
        &mut self,
        generation: u64,
        plan: SnippetRunPlan,
        info: SnippetRunConfirmationInfo,
    ) {
        if self.generation != generation || self.phase != SnippetRunPhase::Preparing {
            return;
        }
        self.confirmation = Some(info);
        self.pending_plan = Some(plan);
        self.phase = SnippetRunPhase::AwaitingConfirmation;
        self.status = Some(
            crate::locale::text(
                "请核对捕获的目标与命令预览后确认",
                "Review the captured target and command preview before confirming",
            )
            .to_string(),
        );
    }

    /// 仅从同一代准备态切换为运行态；取消已先到达时丢弃 plan 而不提交。
    fn begin_execution(&mut self, generation: u64, info: SnippetRunConfirmationInfo) -> bool {
        if self.generation != generation
            || !matches!(
                self.phase,
                SnippetRunPhase::Preparing | SnippetRunPhase::AwaitingConfirmation
            )
            || self
                .cancellation
                .as_ref()
                .is_none_or(CancellationToken::is_cancelled)
        {
            return false;
        }
        self.confirmation = Some(info);
        self.pending_plan = None;
        self.preview_identity = None;
        self.phase = SnippetRunPhase::Running;
        self.status = Some(
            crate::locale::text(
                "命令已提交，正在等待终端回执…",
                "Command submitted; waiting for the terminal receipt…",
            )
            .to_string(),
        );
        true
    }

    /// 仅将真实完成结果显示为 Finished；Unknown 会保留原 session 并禁止直接重放。
    ///
    /// @author kongweiguang
    fn finish_result(&mut self, generation: u64, result: SnippetRunResult) {
        if self.generation != generation {
            return;
        }
        self.cancellation = None;
        self.error = None;
        match &result {
            SnippetRunResult::Finished { exit_code, .. } => {
                self.phase = SnippetRunPhase::Finished;
                self.status = Some(
                    crate::locale::text(
                        format!("命令已确认完成，退出码 {exit_code}"),
                        format!("Command completion confirmed with exit code {exit_code}"),
                    )
                    .to_string(),
                );
            }
            SnippetRunResult::Unknown { .. } => {
                self.phase = SnippetRunPhase::OutcomeUnknown;
                self.unknown_session_id = self
                    .confirmation
                    .as_ref()
                    .map(|info| info.session_id.clone());
                self.status = Some(crate::locale::text(
                    "执行结果未知；请检查原终端后再结束本次记录",
                    "Execution outcome is unknown; inspect the original terminal before acknowledging it",
                ).to_string());
            }
        }
        self.result = Some(result);
    }

    /// 区分确定取消、普通失败与可能已提交的未知结果，避免把 source 错误当作未执行。
    fn finish_error(&mut self, generation: u64, error: AppError, execution_started: bool) {
        if self.generation != generation {
            return;
        }
        let code = error.public_code();
        self.error = Some(error.public_message().to_owned());
        self.cancellation = None;
        self.pending_plan = None;
        self.preview_identity = None;
        match code {
            PublicErrorCode::OutcomeUnknown if execution_started => {
                self.phase = SnippetRunPhase::OutcomeUnknown;
                self.unknown_session_id = self
                    .confirmation
                    .as_ref()
                    .map(|info| info.session_id.clone());
                self.status = Some(crate::locale::text(
                    "执行回执未知；先检查捕获的原终端，不能直接重试",
                    "Execution receipt is unknown; inspect the captured terminal before retrying",
                ).to_string());
            }
            PublicErrorCode::Cancelled => {
                self.phase = SnippetRunPhase::Cancelled;
                self.status = Some(
                    crate::locale::text("本次运行已确认取消", "This run was confirmed cancelled")
                        .to_string(),
                );
            }
            _ => {
                self.phase = SnippetRunPhase::Failed;
                self.status = Some(
                    crate::locale::text("片段运行未完成", "Snippet run did not complete")
                        .to_string(),
                );
            }
        }
    }

    /// 只在任务回执明确为取消时释放忙状态，不以点击取消代替实际收尾。
    fn finish_cancelled(&mut self, generation: u64) {
        if self.generation != generation {
            return;
        }
        self.pending_plan = None;
        self.preview_identity = None;
        self.cancellation = None;
        self.phase = SnippetRunPhase::Cancelled;
        self.status = Some(
            crate::locale::text("本次运行已确认取消", "This run was confirmed cancelled")
                .to_string(),
        );
        self.error = None;
    }

    /// 发送协作取消请求但继续保留任务 owner，直到 application 返回真实结果。
    fn request_cancel(&mut self) -> bool {
        match self.phase {
            SnippetRunPhase::AwaitingConfirmation => {
                if let Some(cancellation) = self.cancellation.take() {
                    cancellation.cancel();
                }
                self.pending_plan = None;
                self.preview_identity = None;
                self.phase = SnippetRunPhase::Cancelled;
                self.status = Some(
                    crate::locale::text(
                        "待确认运行已取消，命令未提交",
                        "Pending run cancelled; no command was submitted",
                    )
                    .to_string(),
                );
                self.error = None;
                true
            }
            SnippetRunPhase::Preparing | SnippetRunPhase::Running => {
                if let Some(cancellation) = &self.cancellation {
                    cancellation.cancel();
                }
                self.phase = SnippetRunPhase::CancellationRequested;
                self.preview_identity = None;
                self.status = Some(
                    crate::locale::text(
                        "已请求取消，正在等待终端确认结果…",
                        "Cancellation requested; waiting for the terminal to confirm the result…",
                    )
                    .to_string(),
                );
                true
            }
            SnippetRunPhase::CancellationRequested => false,
            _ => false,
        }
    }

    /// 关闭只隐藏当前弹层；待运行 future 和其取消令牌仍由状态及 executor 持有。
    fn close_modal(&mut self) {
        self.modal_open = false;
    }

    /// 用户明确检查原终端后解除未知结果锁，不将未知状态伪装成成功或取消。
    fn acknowledge_unknown_after_review(&mut self) -> bool {
        if self.phase != SnippetRunPhase::OutcomeUnknown {
            return false;
        }
        self.phase = SnippetRunPhase::OutcomeReviewed;
        self.unknown_session_id = None;
        self.status = Some(
            crate::locale::text(
                "已由你核对原终端；现在可以开始新的运行",
                "You reviewed the original terminal; a new run may now be started",
            )
            .to_string(),
        );
        true
    }

    /// 显示本地前置校验错误，保留可编辑草稿且不生成应用运行计划。
    fn reject_preparation(&mut self, message: String) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.phase = SnippetRunPhase::Failed;
        self.modal_open = true;
        self.pending_plan = None;
        self.confirmation = None;
        self.preview_identity = None;
        self.result = None;
        self.cancellation = None;
        self.unknown_session_id = None;
        self.error = Some(message);
        self.status = Some(
            crate::locale::text(
                "尚未提交命令，请修正后重新预览",
                "No command was submitted; fix the issue and prepare a new preview",
            )
            .to_string(),
        );
    }
}

/// 待确认命令预览绑定到产生它的目录项版本和可见终端代次。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SnippetPreviewIdentity {
    key: SnippetPreferenceKey,
    revision: Option<ConfigRevision>,
    binding: UiSessionBinding,
}

impl SnippetPreviewIdentity {
    /// 身份值随准备请求复制，避免 renderer 通过后来选中的 Tab 推断旧 plan 的来源。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        key: SnippetPreferenceKey,
        revision: Option<ConfigRevision>,
        binding: UiSessionBinding,
    ) -> Self {
        Self {
            key,
            revision,
            binding,
        }
    }

    /// 任何来源、版本或 session generation 不同都回退为原始模板。
    ///
    /// @author kongweiguang
    fn matches(
        &self,
        key: &SnippetPreferenceKey,
        revision: Option<&ConfigRevision>,
        binding: &UiSessionBinding,
    ) -> bool {
        &self.key == key && self.revision.as_ref() == revision && &self.binding == binding
    }
}

impl CommandsView {
    /// 从已选目录项、当前 runtime 变量和精确 UI binding 创建不可变 plan；按下运行才启动此流程。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_snippet_run(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_terminal_fill_blocked() {
            self.status = Some("请先完成填入，或核对原终端的填入结果".to_owned());
            cx.notify();
            return;
        }
        match self.snippet_run.phase() {
            SnippetRunPhase::AwaitingConfirmation => {
                self.snippet_run.modal_open = true;
                cx.notify();
                return;
            }
            SnippetRunPhase::OutcomeUnknown => {
                self.snippet_run.modal_open = true;
                self.snippet_run.status = Some(
                    crate::locale::text(
                        "请先检查上次运行的原终端并明确结束本次记录",
                        "Inspect the original terminal and acknowledge the previous run first",
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
            phase
                if !matches!(
                    phase,
                    SnippetRunPhase::Idle
                        | SnippetRunPhase::Finished
                        | SnippetRunPhase::OutcomeReviewed
                        | SnippetRunPhase::Cancelled
                        | SnippetRunPhase::Failed
                ) =>
            {
                return;
            }
            _ => {}
        }

        let Some(service) = self.snippet_execution.clone() else {
            self.snippet_run.reject_preparation(
                crate::locale::text(
                    "片段运行服务尚未装配",
                    "Snippet execution service is unavailable",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let Some(selected) = self.selected_snippet.clone() else {
            self.snippet_run.reject_preparation(
                crate::locale::text("请先选择一个已保存的片段", "Select a saved snippet first")
                    .to_string(),
            );
            cx.notify();
            return;
        };
        if self.snippet_is_new {
            self.snippet_run.reject_preparation(
                crate::locale::text(
                    "新建片段尚未保存，不能运行未保存定义",
                    "Save the new snippet before running its definition",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(binding) = self.ui_session_binding.clone() else {
            self.snippet_run.reject_preparation(
                crate::locale::text(
                    "需要明确绑定一个可见终端会话",
                    "Bind a visible terminal session before running a snippet",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        if self.session_id.as_ref() != Some(&binding.session_id) {
            self.snippet_run.reject_preparation(
                crate::locale::text(
                    "终端绑定已变化，请刷新目标后重试",
                    "The terminal binding changed; refresh the target and try again",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let draft = match self.collect_snippet_draft_values(cx) {
            Ok(draft) => draft,
            Err(message) => {
                self.snippet_run.reject_preparation(message);
                cx.notify();
                return;
            }
        };
        if !definition_matches_saved(&selected.definition, &draft) {
            self.snippet_run.reject_preparation(crate::locale::text(
                "片段定义有未保存修改，请先保存或丢弃后再运行",
                "The snippet definition has unsaved changes; save or discard them before running",
            ).to_string());
            cx.notify();
            return;
        }

        let origin = if selected.revision.is_some() {
            SnippetCatalogOrigin::User
        } else {
            SnippetCatalogOrigin::Builtin
        };
        let key = SnippetPreferenceKey {
            origin,
            snippet_id: selected.definition.id.clone(),
        };
        // 使用定义中已验证的变量名，不让正在编辑的名称字段把输入值映射到另一变量。
        let variables = self
            .snippet_variables
            .iter()
            .map(|field| {
                (
                    field.variable.name.clone(),
                    field.input.update(cx, |input, _| input.value().to_string()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let preview_identity =
            SnippetPreviewIdentity::new(key.clone(), selected.revision.clone(), binding.clone());
        let request = SnippetRunPreparation {
            key,
            binding,
            variables,
            record_history: self.record_command,
        };
        let Some((generation, cancellation)) = self
            .snippet_run
            .begin_preparation_with_preview(preview_identity)
        else {
            return;
        };
        let executor = self.executor.clone();
        let prepare_service = service.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(
                    cancellation.clone(),
                    move |operation_cancellation| async move {
                        if operation_cancellation.is_cancelled() {
                            return Err(AppError::cancelled("片段运行预览已取消"));
                        }
                        prepare_service.prepare_run(request).await
                    },
                )
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.snippet_run.generation != generation {
                    return;
                }
                if cancellation.is_cancelled() {
                    view.snippet_run.finish_cancelled(generation);
                    cx.notify();
                    return;
                }
                match result {
                    Ok(plan) => {
                        let info = plan.confirmation().clone();
                        if info.requires_confirmation {
                            view.snippet_run.await_confirmation(generation, plan, info);
                        } else {
                            view.start_snippet_execution(plan, None, info, generation, cx);
                        }
                    }
                    Err(error) => view.snippet_run.finish_error(generation, error, false),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 只确认当前待确认 plan 的一次性令牌，不重新读取当前选项或当前 Tab 来替换目标。
    pub(super) fn confirm_snippet_run(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_run.phase != SnippetRunPhase::AwaitingConfirmation {
            return;
        }
        let Some(plan) = self.snippet_run.pending_plan.take() else {
            self.snippet_run.reject_preparation(
                crate::locale::text(
                    "运行预览已失效，请重新准备",
                    "The run preview expired; prepare it again",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let info = plan.confirmation().clone();
        let confirmation = plan.confirm();
        let generation = self.snippet_run.generation;
        self.start_snippet_execution(plan, Some(confirmation), info, generation, cx);
    }

    /// 请求取消并保留在途用例直至真实回执；未知结果仍进入单独的检查门禁。
    pub(super) fn cancel_snippet_run(&mut self, cx: &mut Context<Self>) {
        if self.snippet_run.request_cancel() {
            cx.notify();
        }
    }

    /// 关闭只收起确认或结果面板，不释放运行 future、捕获目标或 unknown gate。
    pub(super) fn close_snippet_run_modal(&mut self, cx: &mut Context<Self>) {
        self.snippet_run.close_modal();
        cx.notify();
    }

    /// 用户确认检查过原 session 后才解除 Unknown 锁；不将这一步等同命令成功。
    pub(super) fn acknowledge_unknown_after_terminal_review(&mut self, cx: &mut Context<Self>) {
        if self.snippet_run.acknowledge_unknown_after_review() {
            cx.notify();
        }
    }

    /// 暴露只读状态给同一 Commands 面板的 renderer，避免 renderer 自行维护第二份执行状态。
    pub(super) fn snippet_run_state(&self) -> &SnippetRunUiState {
        &self.snippet_run
    }

    /// 启动唯一应用用例并保留 task owner；提交后绝不因窗口或弹层关闭而 drop future。
    fn start_snippet_execution(
        &mut self,
        plan: SnippetRunPlan,
        confirmation: Option<SnippetRunConfirmation>,
        info: SnippetRunConfirmationInfo,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = self.snippet_execution.clone() else {
            self.snippet_run.finish_error(
                generation,
                AppError::unsupported("片段运行服务尚未装配"),
                false,
            );
            return;
        };
        let Some(cancellation) = self.snippet_run.cancellation.clone() else {
            self.snippet_run
                .finish_error(generation, AppError::cancelled("片段运行已取消"), false);
            return;
        };
        if !self.snippet_run.begin_execution(generation, info) {
            self.snippet_run.finish_cancelled(generation);
            return;
        }
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result =
                run_snippet_plan(executor, service, plan, confirmation, cancellation).await;
            let _ = this.update(cx, |view, cx| {
                view.snippet_run.finish_run_result(generation, result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl SnippetRunUiState {
    /// 执行态结果共享一个入口以便同步释放取消令牌并保留 Unknown 的人工核对信息。
    fn finish_run_result(&mut self, generation: u64, result: Result<SnippetRunResult, AppError>) {
        match result {
            Ok(result) => self.finish_result(generation, result),
            Err(error) => self.finish_error(generation, error, true),
        }
    }
}

/// 应用 executor 在同一任务内等待协作取消后的实际终端回执，不使用外层 select 丢 future。
async fn run_snippet_plan(
    executor: ApplicationTaskExecutor,
    service: Arc<SnippetExecutionService>,
    plan: SnippetRunPlan,
    confirmation: Option<SnippetRunConfirmation>,
    cancellation: CancellationToken,
) -> Result<SnippetRunResult, AppError> {
    executor
        .run_cancellable(cancellation, move |operation_cancellation| async move {
            service
                .run(plan, confirmation, operation_cancellation)
                .await
        })
        .await
}

/// Run 与 Fill/外部变更检查共用定义比较，避免 UI 隐式 Global binding 被误判为改动。
///
/// @author kongweiguang
fn definition_matches_saved(saved: &SnippetDefinition, draft: &SnippetDraftValues) -> bool {
    super::configuration_watch::snippet_definition_draft_matches_saved(saved, draft)
}

#[cfg(test)]
#[path = "../../../tests/unit/commands_snippet_run.rs"]
mod tests;
