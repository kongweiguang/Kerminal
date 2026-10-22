//! 当前选中片段的适用性只读查询与迟到回执隔离。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError, ApplicationTaskExecutor,
    commands::{CommandsService, SnippetApplicabilityProjection},
    sessions::UiSessionBinding,
};
use kerminal_domain::{
    commands::{SnippetApplicabilityStatus, SnippetCatalogOrigin, SnippetPreferenceKey},
    preferences::ConfigRevision,
};
use tokio_util::sync::CancellationToken;

use super::{CommandsView, command_error_context};

/// 查询身份包含文件 revision 与 exact UI binding，防止旧定义或重连目标覆盖新详情。
#[derive(Clone, Debug, Eq, PartialEq)]
struct ApplicabilityIdentity {
    key: SnippetPreferenceKey,
    revision: Option<ConfigRevision>,
    binding: Option<UiSessionBinding>,
}

/// 仅承载当前选择的请求阶段；失败和等待不会被解释为兼容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SnippetApplicabilityPhase {
    Loading,
    Ready(SnippetApplicabilityProjection),
    Failed(String),
}

#[derive(Clone)]
struct ApplicabilityTicket {
    generation: u64,
    identity: ApplicabilityIdentity,
    cancellation: CancellationToken,
}

#[derive(Default)]
pub(super) struct SelectedSnippetApplicability {
    generation: u64,
    cancellation: Option<CancellationToken>,
    current: Option<(ApplicabilityIdentity, SnippetApplicabilityPhase)>,
}

impl SelectedSnippetApplicability {
    /// 每次新选择或新 binding 都替换旧请求，并立即清除旧判定。
    ///
    /// @author kongweiguang
    fn begin(&mut self, identity: ApplicabilityIdentity) -> ApplicabilityTicket {
        self.cancel();
        self.generation = self.generation.wrapping_add(1).max(1);
        let cancellation = CancellationToken::new();
        self.cancellation = Some(cancellation.clone());
        self.current = Some((identity.clone(), SnippetApplicabilityPhase::Loading));
        ApplicabilityTicket {
            generation: self.generation,
            identity,
            cancellation,
        }
    }

    /// 选择离开或 binding 变化时立即取消并删除旧投影，避免短暂显示错误终端结论。
    ///
    /// @author kongweiguang
    fn clear(&mut self) {
        self.cancel();
        self.generation = self.generation.wrapping_add(1).max(1);
        self.current = None;
    }

    /// 仅当前选择、revision、binding 和请求代次都匹配时才收下只读结果。
    ///
    /// @author kongweiguang
    fn finish(
        &mut self,
        ticket: &ApplicabilityTicket,
        current_identity: Option<ApplicabilityIdentity>,
        result: Result<SnippetApplicabilityProjection, String>,
    ) -> bool {
        if ticket.cancellation.is_cancelled()
            || self.generation != ticket.generation
            || current_identity.as_ref() != Some(&ticket.identity)
            || self
                .current
                .as_ref()
                .is_none_or(|(identity, _)| identity != &ticket.identity)
        {
            return false;
        }

        self.cancellation = None;
        self.current = Some((
            ticket.identity.clone(),
            match result {
                Ok(projection) => SnippetApplicabilityPhase::Ready(projection),
                Err(message) => SnippetApplicabilityPhase::Failed(message),
            },
        ));
        true
    }

    /// 新票据取代时只取消自己的读取任务，不影响其他面板的执行。
    ///
    /// @author kongweiguang
    fn cancel(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
    }

    /// 渲染侧只读取与此刻 selected definition 和 binding 完全相同的阶段。
    ///
    /// @author kongweiguang
    fn phase_for(&self, identity: &ApplicabilityIdentity) -> Option<&SnippetApplicabilityPhase> {
        self.current
            .as_ref()
            .filter(|(current, _)| current == identity)
            .map(|(_, phase)| phase)
    }
}

/// 对当前 exact selection 发起一次可取消只读查询；列表行只使用已有 catalog 声明。
///
/// @author kongweiguang
pub(super) fn refresh_selected(view: &mut CommandsView, cx: &mut gpui_kit::Context<CommandsView>) {
    let Some(identity) = selected_identity(view) else {
        clear(view);
        cx.notify();
        return;
    };

    let ticket = view.snippet_applicability.begin(identity.clone());
    let request_cancellation = ticket.cancellation.clone();
    let commands: std::sync::Arc<CommandsService> = view.commands.clone();
    let executor: ApplicationTaskExecutor = view.executor.clone();
    let request_key = identity.key.clone();
    let request_binding = identity.binding.clone();
    let completion_ticket = ticket.clone();

    cx.spawn(async move |this, cx| {
        let result = executor
            .run_cancellable(request_cancellation, move |cancellation| async move {
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled(command_error_context(
                        "片段适用性查询已取消",
                        "Snippet applicability query was cancelled",
                    )));
                }
                let result = commands
                    .snippet_applicability(request_key, request_binding)
                    .await?;
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled(command_error_context(
                        "片段适用性查询已取消",
                        "Snippet applicability query was cancelled",
                    )));
                }
                result.ok_or_else(|| {
                    AppError::not_found(command_error_context(
                        "片段已从目录移除，请重试读取",
                        "The snippet is no longer in the catalog; retry the read",
                    ))
                })
            })
            .await;

        let _ = this.update(cx, |view, cx| {
            let current_identity = selected_identity(view);
            let result = result.map_err(|error| error.public_message().to_owned());
            if view
                .snippet_applicability
                .finish(&completion_ticket, current_identity, result)
            {
                cx.notify();
            }
        });
    })
    .detach();
    cx.notify();
}

/// 清除 selection 离开时的唯一适用性状态；迟到结果会因代次失效。
///
/// @author kongweiguang
pub(super) fn clear(view: &mut CommandsView) {
    view.snippet_applicability.clear();
}

/// 重试只重放当前 selection 的只读判定，不改变运行计划或执行状态。
///
/// @author kongweiguang
pub(super) fn retry(view: &mut CommandsView, cx: &mut gpui_kit::Context<CommandsView>) {
    refresh_selected(view, cx);
}

impl CommandsView {
    /// 以当前已保存的来源、revision 与 binding 读取详情状态，草稿字段不改变目录身份。
    ///
    /// @author kongweiguang
    pub(super) fn selected_snippet_applicability(&self) -> Option<&SnippetApplicabilityPhase> {
        let identity = selected_identity(self)?;
        self.snippet_applicability.phase_for(&identity)
    }

    /// 只有当前已读取且明确不兼容时禁用运行；未知或读取失败仍由应用 prepare/commit 决定。
    ///
    /// @author kongweiguang
    pub(super) fn selected_snippet_is_incompatible(&self) -> bool {
        is_confirmed_incompatible(self.selected_snippet_applicability())
    }

    /// 详情中的错误按钮只会重新查询当前 item，不继承旧绑定或其他列表项。
    ///
    /// @author kongweiguang
    pub(super) fn retry_selected_snippet_applicability(
        &mut self,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        retry(self, cx);
    }
}

/// 从唯一选中定义构造 UI 票据；origin 由保存 revision 决定，不按 derived_from 猜来源。
///
/// @author kongweiguang
fn selected_identity(view: &CommandsView) -> Option<ApplicabilityIdentity> {
    let selected = view.selected_snippet.as_ref()?;
    let origin = if selected.revision.is_some() {
        SnippetCatalogOrigin::User
    } else {
        SnippetCatalogOrigin::Builtin
    };
    Some(ApplicabilityIdentity {
        key: SnippetPreferenceKey {
            origin,
            snippet_id: selected.definition.id.clone(),
        },
        revision: selected.revision.clone(),
        binding: view.ui_session_binding.clone(),
    })
}

/// Pending、失败和 Unknown 都不能当作兼容或不兼容；只有可信评估明确冲突才阻止运行。
///
/// @author kongweiguang
fn is_confirmed_incompatible(phase: Option<&SnippetApplicabilityPhase>) -> bool {
    matches!(
        phase,
        Some(SnippetApplicabilityPhase::Ready(projection))
            if projection.assessment.status == SnippetApplicabilityStatus::Incompatible
    )
}

#[cfg(test)]
#[path = "../../../tests/unit/commands_snippet_applicability.rs"]
mod tests;
