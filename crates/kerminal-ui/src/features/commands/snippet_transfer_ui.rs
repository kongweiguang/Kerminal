//! 片段 JSON 交换与目录管理的 UI 协调；文件 I/O 和批次语义留在 application。
//!
//! @author kongweiguang

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use gpui_kit::{Context, PathPromptOptions, Window};
use kerminal_application::{
    AppError, PublicErrorCode,
    commands::{SnippetImportBatchRequest, SnippetImportBatchState, SnippetImportPreview},
    configuration_directory::ConfigurationDirectoryKind,
    preferences::ConfigurationValidationScope,
};
use kerminal_domain::commands::CommandHistoryListRequest;
use tokio_util::sync::CancellationToken;

use super::CommandsView;

const SNIPPET_HISTORY_LIMIT: usize = 20;

/// 导入候选只暴露用户选择所需的安全摘要；原始定义与变量默认值不交给 renderer。
#[derive(Clone, Eq, PartialEq)]
pub(super) struct SnippetImportCandidateRow {
    pub(super) source_index: usize,
    pub(super) title: String,
    pub(super) command: String,
    pub(super) title_conflicts: bool,
    pub(super) selected: bool,
}

/// 导入确认阶段明确区分可编辑预览、未提交、结果未知和已完成。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SnippetImportPhase {
    Reviewing,
    Committing,
    NotCommitted,
    OutcomeUnknown,
    Committed,
}

/// renderer 读取安全摘要与阶段；完整候选和可重试请求仅保留在此 leaf 内存中。
pub(super) struct SnippetImportReview {
    #[cfg(feature = "right-rail-acceptance")]
    pub(super) file_name: String,
    pub(super) candidates: Vec<SnippetImportCandidateRow>,
    pub(super) issues: Vec<String>,
    pub(super) phase: SnippetImportPhase,
    pub(super) status: Option<String>,
    preview: SnippetImportPreview,
    selected_source_indices: BTreeSet<usize>,
    batch_request: Option<SnippetImportBatchRequest>,
    batch_id: Option<String>,
}

impl SnippetImportReview {
    /// 由摘要行统计选择数，不让 UI 读取或复制含有敏感默认值的完整定义。
    pub(super) fn selected_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.selected)
            .count()
    }

    /// 写入批次正在提交或结果未知时锁住选择，确保重试仍指向同一组定义。
    ///
    /// @author kongweiguang
    fn can_change_selection(&self) -> bool {
        matches!(
            self.phase,
            SnippetImportPhase::Reviewing | SnippetImportPhase::NotCommitted
        )
    }
}

impl CommandsView {
    /// 打开系统文件选择器后由 application 读取并解析；迟到选择结果受 transfer generation 隔离。
    ///
    /// @author kongweiguang
    pub fn preview_snippet_import(&mut self, cx: &mut Context<Self>) {
        if self.snippet_transfer_busy {
            return;
        }
        if self.snippet_import_review.as_ref().is_some_and(|review| {
            matches!(
                review.phase,
                SnippetImportPhase::Committing | SnippetImportPhase::OutcomeUnknown
            )
        }) {
            self.status = Some(transfer_message(
                "请先查询并确认当前导入批次状态，再开始新的预览。",
                "Reconcile the current import batch before previewing another file.",
            ));
            cx.notify();
            return;
        }
        if self.snippet_transfer_files.is_none() {
            self.status = Some(transfer_message(
                "片段文件服务尚未装配。",
                "The snippet file service is unavailable.",
            ));
            cx.notify();
            return;
        }

        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::locale::text(
                "选择要预览的 JSON v1 片段文件",
                "Choose a JSON v1 snippet file to preview",
            )),
        });
        let (generation, cancellation) = self.begin_snippet_transfer();
        self.status = Some(transfer_message(
            "请选择片段文件…",
            "Choose a snippet file…",
        ));
        let cancellation_for_picker = cancellation.clone();
        cx.spawn(async move |this, cx| {
            let selected = selected.await;
            let _ = this.update(cx, |view, cx| {
                if view.snippet_transfer_generation != generation
                    || cancellation_for_picker.is_cancelled()
                {
                    return;
                }
                match selected {
                    Ok(Ok(Some(mut paths))) => {
                        if let Some(path) = paths.pop() {
                            view.read_snippet_import(path, generation, cancellation, cx);
                        } else {
                            view.finish_snippet_transfer(generation);
                            view.status = Some(transfer_message(
                                "文件选择器未返回文件路径。",
                                "The file picker returned no file path.",
                            ));
                            cx.notify();
                        }
                    }
                    Ok(Ok(None)) => {
                        view.finish_snippet_transfer(generation);
                        view.status = None;
                        cx.notify();
                    }
                    Ok(Err(_)) | Err(_) => {
                        view.finish_snippet_transfer(generation);
                        view.status = Some(transfer_message(
                            "无法读取所选文件路径，请重试。",
                            "Could not read the selected file path. Try again.",
                        ));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Acceptance 直接送入相同文件读取与 application 预览链，避免打开系统 picker 或重复触发在途请求。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn preview_snippet_import_path_for_acceptance(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.snippet_transfer_busy {
            return Ok(false);
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("snippets.json");
        if let Some(review) = self.snippet_import_review.as_ref() {
            if matches!(
                review.phase,
                SnippetImportPhase::Committing | SnippetImportPhase::OutcomeUnknown
            ) {
                return Err(review.status.clone().unwrap_or_else(|| {
                    transfer_message(
                        "请先查询并确认当前导入批次状态，再开始新的预览。",
                        "Reconcile the current import batch before previewing another file.",
                    )
                }));
            }
            if review.file_name == file_name && review.phase == SnippetImportPhase::Reviewing {
                return Ok(false);
            }
        }
        if self.snippet_transfer_files.is_none() {
            return Err(transfer_message(
                "片段文件服务尚未装配。",
                "The snippet file service is unavailable.",
            ));
        }

        let (generation, cancellation) = self.begin_snippet_transfer();
        self.read_snippet_import(path, generation, cancellation, cx);
        cx.notify();
        Ok(true)
    }

    /// 只接受指定文件的单候选安全预览，未完成读取时返回 false 供原生验收继续等待。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn snippet_import_review_ready_for_acceptance(
        &self,
        expected_file_name: &str,
    ) -> Result<bool, String> {
        if self.snippet_transfer_busy {
            return Ok(false);
        }
        let Some(review) = self.snippet_import_review.as_ref() else {
            return self
                .status
                .as_ref()
                .map_or(Ok(false), |status| Err(status.clone()));
        };
        if review.file_name != expected_file_name {
            return Err(transfer_message(
                "导入文件与预期样本不一致。",
                "The import file does not match the expected sample.",
            ));
        }
        if review.phase != SnippetImportPhase::Reviewing {
            return Err(review.status.clone().unwrap_or_else(|| {
                transfer_message(
                    "导入预览尚未进入可确认状态。",
                    "The import preview is not ready for review.",
                )
            }));
        }
        if review.candidates.len() != 1 || !review.issues.is_empty() {
            return Err(review.status.clone().unwrap_or_else(|| {
                transfer_message(
                    "导入预览未通过单候选验收条件。",
                    "The import preview does not match the single-candidate acceptance case.",
                )
            }));
        }
        Ok(true)
    }

    /// 用户修改选择后旧批次请求不再复用；Unknown 阶段只能先查询提交状态。
    ///
    /// @author kongweiguang
    pub fn set_snippet_import_candidate_selected(
        &mut self,
        source_index: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.snippet_import_review.as_mut() else {
            return;
        };
        if !review.can_change_selection() {
            return;
        }
        let Some(row) = review
            .candidates
            .iter_mut()
            .find(|candidate| candidate.source_index == source_index)
        else {
            return;
        };
        if row.selected == selected {
            return;
        }
        row.selected = selected;
        if selected {
            review.selected_source_indices.insert(source_index);
        } else {
            review.selected_source_indices.remove(&source_index);
        }
        review.batch_request = None;
        review.batch_id = None;
        review.phase = SnippetImportPhase::Reviewing;
        review.status = None;
        cx.notify();
    }

    /// 提交一次用户确认的完整批次；仅明确 NotCommitted 才能重用同一批次请求。
    ///
    /// @author kongweiguang
    pub fn confirm_snippet_import(&mut self, cx: &mut Context<Self>) {
        let Some(review) = self.snippet_import_review.as_ref() else {
            return;
        };
        if self.snippet_transfer_busy
            || !matches!(
                review.phase,
                SnippetImportPhase::Reviewing | SnippetImportPhase::NotCommitted
            )
        {
            return;
        }

        let request = match review.batch_request.clone() {
            Some(request) if review.phase == SnippetImportPhase::NotCommitted => request,
            _ => {
                let definitions = review
                    .preview
                    .candidates
                    .iter()
                    .filter(|candidate| {
                        review
                            .selected_source_indices
                            .contains(&candidate.source_index)
                    })
                    .map(|candidate| candidate.definition.clone())
                    .collect::<Vec<_>>();
                if definitions.is_empty() {
                    self.status = Some(transfer_message(
                        "请至少选择一个有效片段。",
                        "Select at least one valid snippet.",
                    ));
                    cx.notify();
                    return;
                }
                SnippetImportBatchRequest {
                    batch_id: self.commands.new_definition_id(),
                    definitions,
                }
            }
        };
        let batch_id = request.batch_id.clone();
        if let Some(review) = self.snippet_import_review.as_mut() {
            review.batch_id = Some(batch_id);
            review.batch_request = Some(request.clone());
            review.phase = SnippetImportPhase::Committing;
            review.status = Some(transfer_message(
                "正在提交所选片段…",
                "Importing selected snippets…",
            ));
        }

        let (generation, cancellation) = self.begin_snippet_transfer();
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |_| async move {
                    commands.import_snippet_batch(request).await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.finish_snippet_transfer(generation) {
                    return;
                }
                view.apply_snippet_import_result(result, generation, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Unknown 结果只查 journal；此查询失败时保留 Unknown 并禁止重复提交。
    ///
    /// @author kongweiguang
    pub fn reconcile_snippet_import(&mut self, cx: &mut Context<Self>) {
        let Some(review) = self.snippet_import_review.as_ref() else {
            return;
        };
        if self.snippet_transfer_busy || review.phase != SnippetImportPhase::OutcomeUnknown {
            return;
        }
        let Some(batch_id) = review.batch_id.clone() else {
            return;
        };
        let (generation, cancellation) = self.begin_snippet_transfer();
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        if let Some(review) = self.snippet_import_review.as_mut() {
            review.status = Some(transfer_message(
                "正在查询导入批次状态…",
                "Checking the import batch status…",
            ));
        }
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |_| async move {
                    commands.snippet_import_batch_status(batch_id).await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.finish_snippet_transfer(generation) {
                    return;
                }
                view.apply_snippet_import_result(result, generation, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// 关闭预览仅丢弃未提交选择；提交中或结果未知时保留批次身份供后续查询。
    ///
    /// @author kongweiguang
    pub fn cancel_snippet_import_review(&mut self, cx: &mut Context<Self>) {
        if self.snippet_transfer_busy
            || self.snippet_import_review.as_ref().is_some_and(|review| {
                matches!(
                    review.phase,
                    SnippetImportPhase::Committing | SnippetImportPhase::OutcomeUnknown
                )
            })
        {
            return;
        }
        self.snippet_import_review = None;
        self.status = None;
        cx.emit(super::SnippetEditorVisibilityChanged {
            open: self.snippet_overlay_open(),
            focus_command_draft: false,
        });
        cx.notify();
    }

    /// 导出始终从 application 读取完整 User 目录，不受当前分类、搜索或分页影响。
    ///
    /// @author kongweiguang
    pub fn export_snippets(&mut self, cx: &mut Context<Self>) {
        if self.snippet_transfer_busy {
            return;
        }
        if self.snippet_transfer_files.is_none() {
            self.status = Some(transfer_message(
                "片段文件服务尚未装配。",
                "The snippet file service is unavailable.",
            ));
            cx.notify();
            return;
        }
        let selected = cx.prompt_for_new_path(Path::new("."), Some("kerminal-snippets.json"));
        let (generation, cancellation) = self.begin_snippet_transfer();
        self.status = Some(transfer_message(
            "请选择导出文件位置…",
            "Choose where to save the export…",
        ));
        let cancellation_for_picker = cancellation.clone();
        let executor = self.executor.clone();
        let commands = self.commands.clone();
        let Some(files) = self.snippet_transfer_files.clone() else {
            self.finish_snippet_transfer(generation);
            self.status = Some(transfer_message(
                "片段文件服务尚未装配。",
                "The snippet file service is unavailable.",
            ));
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let selected = selected.await;
            let path = match selected {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => {
                    let _ = this.update(cx, |view, cx| {
                        if view.finish_snippet_transfer(generation) {
                            view.status = None;
                            cx.notify();
                        }
                    });
                    return;
                }
                Ok(Err(_)) | Err(_) => {
                    let _ = this.update(cx, |view, cx| {
                        if view.finish_snippet_transfer(generation) {
                            view.status = Some(transfer_message(
                                "无法读取导出文件路径，请重试。",
                                "Could not read the export path. Try again.",
                            ));
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            if cancellation_for_picker.is_cancelled() {
                return;
            }
            let result = executor
                .run_cancellable(cancellation.clone(), move |operation_cancel| async move {
                    let source = commands.export_user_snippets().await?;
                    if operation_cancel.is_cancelled() {
                        return Err(AppError::cancelled("片段导出已取消"));
                    }
                    files.write_export_file(path, source, operation_cancel).await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.finish_snippet_transfer(generation) {
                    return;
                }
                match result {
                    Ok(receipt) => {
                        view.status = Some(transfer_message(
                            format!("已导出 {} 字节。", receipt.bytes_written),
                            format!("Exported {} bytes.", receipt.bytes_written),
                        ));
                    }
                    Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                        view.status = Some(transfer_message(
                            "导出结果未知；未自动重试，请检查目标文件后再操作。",
                            "The export result is unknown. It was not retried; check the target file before trying again.",
                        ));
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Root 以该联合状态维持唯一模态层，编辑器、管理器与导入复核共用同一 overlay 生命周期。
    ///
    /// @author kongweiguang
    pub(crate) fn snippet_overlay_open(&self) -> bool {
        self.snippet_editor_open()
            || self.snippet_library_manager_open
            || self.snippet_import_review.is_some()
    }

    /// Escape 与遮罩关闭统一分发到最上层弹层，导入提交中和结果未知继续受批次门禁保护。
    ///
    /// @author kongweiguang
    pub(crate) fn close_snippet_overlay(&mut self, cx: &mut Context<Self>) {
        if self.snippet_import_review.is_some() {
            self.cancel_snippet_import_review(cx);
        } else if self.snippet_library_manager_open {
            self.close_snippet_library_manager(cx);
        } else {
            self.handle_snippet_editor_escape(cx);
        }
    }

    /// 将初始焦点交给共享 modal 中实际可见的表单；导入复用 manager focus handle，避免新增 owner 状态。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_snippet_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_editor_open() {
            self.focus_snippet_editor(window, cx);
        } else if self.snippet_import_review.is_some() || self.snippet_library_manager_open {
            window.focus(&self.snippet_manager_focus, cx);
        }
    }

    /// 打开管理器时撤销未完成的新建意图并重置临时查询；目录与 validator 文本仍来自 Root 投影。
    ///
    /// @author kongweiguang
    pub fn open_snippet_library_manager(&mut self, cx: &mut Context<Self>) {
        if self.snippet_editor_open() || self.snippet_library_manager_open {
            return;
        }
        self.cancel_pending_snippet_command_request(cx);
        self.snippet_library_manager_open = true;
        self.snippet_usage_clear_confirm = false;
        self.snippet_history_entries.clear();
        self.snippet_history_loaded = false;
        self.status = None;
        cx.emit(super::SnippetEditorVisibilityChanged {
            open: self.snippet_overlay_open(),
            focus_command_draft: false,
        });
        cx.notify();
    }

    /// 有 pane 时读取其最近命令，否则沿用 main 的全局历史；冻结可空目标以拒绝迟到响应。
    ///
    /// @author kongweiguang
    pub(super) fn load_snippet_history(&mut self, cx: &mut Context<Self>) {
        if self.snippet_history_loading {
            return;
        }
        let target = self.snippet_history_target.clone();

        if let Some(previous) = self.snippet_history_cancellation.take() {
            previous.cancel();
        }
        self.snippet_history_generation = self.snippet_history_generation.wrapping_add(1).max(1);
        let generation = self.snippet_history_generation;
        let cancellation = CancellationToken::new();
        let request = target.as_ref().map_or_else(
            || CommandHistoryListRequest {
                limit: Some(SNIPPET_HISTORY_LIMIT),
                ..Default::default()
            },
            |target| target.snippet_history_request(SNIPPET_HISTORY_LIMIT),
        );
        let service = self.commands.clone();
        let executor = self.executor.clone();
        self.snippet_history_cancellation = Some(cancellation.clone());
        self.snippet_history_entries.clear();
        self.snippet_history_loaded = false;
        self.snippet_history_loading = true;
        self.status = Some(transfer_message(
            "正在读取当前分屏的命令历史…",
            "Loading command history for this pane…",
        ));
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |_| async move {
                    service.list_history(request).await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.snippet_history_generation != generation
                    || view.snippet_history_target != target
                    || cancellation.is_cancelled()
                {
                    return;
                }
                view.snippet_history_cancellation = None;
                view.snippet_history_loading = false;
                match result {
                    Ok(entries) => {
                        view.snippet_history_entries = entries
                            .into_iter()
                            .filter(|entry| !entry.command.trim().is_empty())
                            .collect();
                        view.snippet_history_loaded = true;
                        view.status = view.snippet_history_entries.is_empty().then(|| {
                            transfer_message(
                                "当前没有可保存的命令历史。",
                                "There is no command history to save.",
                            )
                        });
                    }
                    Err(_) => {
                        view.snippet_history_entries.clear();
                        view.snippet_history_loaded = false;
                        view.status = Some(transfer_message(
                            "命令历史暂时无法读取，请稍后重试。",
                            "Command history could not be loaded. Try again later.",
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 只接受仍属于当前冻结可空目标的历史行，再走统一脏稿门禁创建可编辑副本。
    ///
    /// @author kongweiguang
    pub(super) fn select_snippet_history_entry(
        &mut self,
        entry_id: &str,
        target: Option<&super::LogsTarget>,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_history_loading
            || !self.snippet_history_loaded
            || self.snippet_history_target.as_ref() != target
        {
            self.status = Some(transfer_message(
                "终端分屏已变化，请重新读取命令历史。",
                "The terminal pane changed. Reload command history.",
            ));
            cx.notify();
            return;
        }
        let Some(command) = self
            .snippet_history_entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .map(|entry| entry.command.clone())
        else {
            self.status = Some(transfer_message(
                "这条命令历史已过期，请重新读取。",
                "This command history entry is stale. Reload the list.",
            ));
            cx.notify();
            return;
        };
        if !self.close_snippet_library_manager_for_editor(cx) {
            return;
        }
        self.request_new_snippet(Some(command), window, cx);
    }

    /// 清理前先展示影响范围；收藏保留由 application repository 的原子契约保证。
    ///
    /// @author kongweiguang
    pub(super) fn request_clear_snippet_usage_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.snippet_usage_clear_running {
            return;
        }
        self.snippet_usage_clear_confirm = true;
        self.status = None;
        cx.notify();
    }

    /// 取消确认只退出决策状态，不触碰收藏或使用记录。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_clear_snippet_usage(&mut self, cx: &mut Context<Self>) {
        if self.snippet_usage_clear_running {
            return;
        }
        self.snippet_usage_clear_confirm = false;
        self.status = None;
        cx.notify();
    }

    /// 仅在二次确认后调用 application 的原子清理用例，并锁住关闭/重复提交直到结果收敛。
    ///
    /// @author kongweiguang
    pub(super) fn clear_snippet_usage(&mut self, cx: &mut Context<Self>) {
        if !self.snippet_usage_clear_confirm || self.snippet_usage_clear_running {
            return;
        }
        self.snippet_usage_clear_generation =
            self.snippet_usage_clear_generation.wrapping_add(1).max(1);
        let generation = self.snippet_usage_clear_generation;
        let service = self.commands.clone();
        let executor = self.executor.clone();
        self.snippet_usage_clear_running = true;
        self.status = Some(transfer_message(
            "正在清除最近使用和次数…",
            "Clearing recent usage and counts…",
        ));
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.clear_snippet_usage().await })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.snippet_usage_clear_generation != generation {
                    return;
                }
                view.snippet_usage_clear_running = false;
                match result {
                    Ok(_) => {
                        view.snippet_usage_clear_confirm = false;
                        view.refresh_snippet_page(cx);
                        view.status = Some(transfer_message(
                            "最近使用和次数已清除，收藏保持不变。",
                            "Recent usage and counts were cleared. Favorites were kept.",
                        ));
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// X、遮罩与 Escape 在操作忙碌时保持弹层，避免把一次普通关闭解释为取消业务。
    ///
    /// @author kongweiguang
    pub fn close_snippet_library_manager(&mut self, cx: &mut Context<Self>) {
        if !self.close_snippet_library_manager_state(false) {
            return;
        }
        cx.emit(super::SnippetEditorVisibilityChanged {
            open: self.snippet_overlay_open(),
            focus_command_draft: false,
        });
        cx.notify();
    }

    /// main 的完成按钮允许离开管理器；已提交写入保留代次与回执，仅读取可撤销。
    ///
    /// @author kongweiguang
    pub(super) fn finish_snippet_library_manager(&mut self, cx: &mut Context<Self>) {
        if self.close_snippet_library_manager_state(true) {
            cx.emit(super::SnippetEditorVisibilityChanged {
                open: self.snippet_overlay_open(),
                focus_command_draft: false,
            });
            cx.notify();
        }
    }

    /// 统一普通关闭与按钮的忙碌判定，防止出现可点击但无法关闭的入口。
    ///
    /// @author kongweiguang
    pub(super) fn snippet_library_manager_busy(&self) -> bool {
        self.snippet_validation_running
            || self.snippet_directory_request_pending
            || self.snippet_history_loading
            || self.snippet_usage_clear_running
    }

    /// 编辑器从管理历史打开时只清理管理状态，不短暂关闭共享 modal 丢失原始返回焦点。
    ///
    /// @author kongweiguang
    fn close_snippet_library_manager_for_editor(&mut self, _cx: &mut Context<Self>) -> bool {
        self.close_snippet_library_manager_state(false)
    }

    /// 关闭时只撤销读取与临时视图；完成按钮可跳过忙碌门禁，已提交写入继续收敛。
    ///
    /// @author kongweiguang
    fn close_snippet_library_manager_state(&mut self, allow_busy: bool) -> bool {
        if !self.snippet_library_manager_open {
            return false;
        }
        if !allow_busy && self.snippet_library_manager_busy() {
            return false;
        }
        if let Some(cancellation) = self.snippet_manager_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.snippet_history_cancellation.take() {
            cancellation.cancel();
        }
        self.snippet_history_generation = self.snippet_history_generation.wrapping_add(1).max(1);
        self.snippet_history_loading = false;
        self.snippet_history_loaded = false;
        self.snippet_history_entries.clear();
        self.snippet_usage_clear_confirm = false;
        self.snippet_manager_generation = self.snippet_manager_generation.wrapping_add(1).max(1);
        self.snippet_validation_running = false;
        self.snippet_directory_request_pending = false;
        self.snippet_library_manager_open = false;
        true
    }

    /// 复用 application validator 报告相对配置路径和安全诊断，不在 UI 复制 TOML 规则。
    ///
    /// @author kongweiguang
    pub fn validate_snippet_configuration(&mut self, cx: &mut Context<Self>) {
        let Some(validation) = self.configuration_validation.clone() else {
            self.status = Some(transfer_message(
                "片段配置校验服务尚未装配。",
                "Snippet configuration validation is unavailable.",
            ));
            cx.notify();
            return;
        };
        let (generation, cancellation) = self.begin_snippet_manager_operation();
        self.snippet_validation_running = true;
        self.status = Some(transfer_message(
            "正在校验片段配置…",
            "Validating snippet configuration…",
        ));
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |_| async move {
                    validation
                        .validate(ConfigurationValidationScope::Snippets)
                        .await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.finish_snippet_manager_operation(generation) {
                    return;
                }
                view.snippet_validation_running = false;
                match result {
                    Ok(report) => {
                        let count = report.checked.len();
                        view.status = Some(transfer_message(
                            format!(
                                "校验完成：检查 {count} 项，{} 个错误，{} 个警告。",
                                report.error_count, report.warning_count
                            ),
                            format!(
                                "Validation complete: {count} checked, {} errors, {} warnings.",
                                report.error_count, report.warning_count
                            ),
                        ));
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 目录按钮只调用固定 Snippets 目录 application port，不接受或拼接用户路径。
    ///
    /// @author kongweiguang
    pub fn open_snippet_configuration_directory(&mut self, cx: &mut Context<Self>) {
        let Some(directory) = self.configuration_directory.clone() else {
            self.status = Some(transfer_message(
                "配置目录打开服务尚未装配。",
                "The configuration directory service is unavailable.",
            ));
            cx.notify();
            return;
        };
        let (generation, cancellation) = self.begin_snippet_manager_operation();
        self.snippet_directory_request_pending = true;
        self.status = Some(transfer_message(
            "正在请求打开片段配置目录…",
            "Requesting the snippets configuration directory…",
        ));
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |operation_cancel| async move {
                    directory
                        .open(ConfigurationDirectoryKind::Snippets, operation_cancel)
                        .await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.finish_snippet_manager_operation(generation) {
                    return;
                }
                view.snippet_directory_request_pending = false;
                view.status = match result {
                    Ok(_) => Some(transfer_message(
                        "系统已接受打开目录的请求；这不代表文件管理器已显示。",
                        "The OS accepted the directory request; this does not confirm a file manager window is visible.",
                    )),
                    Err(error) => Some(error.public_message().to_owned()),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 使用受限 application 文件端口读取并生成安全预览，不在 UI 线程解析磁盘内容。
    ///
    /// @author kongweiguang
    fn read_snippet_import(
        &mut self,
        path: PathBuf,
        generation: u64,
        cancellation: CancellationToken,
        cx: &mut Context<Self>,
    ) {
        let Some(files) = self.snippet_transfer_files.clone() else {
            self.finish_snippet_transfer(generation);
            self.status = Some(transfer_message(
                "片段文件服务尚未装配。",
                "The snippet file service is unavailable.",
            ));
            cx.notify();
            return;
        };
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        #[cfg(feature = "right-rail-acceptance")]
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("snippets.json")
            .to_owned();
        self.status = Some(transfer_message(
            "正在读取并检查片段文件…",
            "Reading and checking the snippet file…",
        ));
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |operation_cancel| async move {
                    let source = files
                        .read_import_file(path, operation_cancel.clone())
                        .await?;
                    if operation_cancel.is_cancelled() {
                        return Err(AppError::cancelled("片段预览已取消"));
                    }
                    let preview = commands.preview_snippet_import(&source).await?;
                    Ok(preview)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.finish_snippet_transfer(generation) {
                    return;
                }
                if cancellation.is_cancelled() {
                    view.status = None;
                    cx.notify();
                    return;
                }
                match result {
                    Ok(preview) => {
                        let candidates = preview
                            .candidates
                            .iter()
                            .map(|candidate| SnippetImportCandidateRow {
                                source_index: candidate.source_index,
                                title: candidate.definition.title.clone(),
                                command: candidate.definition.command.clone(),
                                title_conflicts: candidate.title_conflicts,
                                selected: true,
                            })
                            .collect::<Vec<_>>();
                        let selected_source_indices = candidates
                            .iter()
                            .filter(|candidate| candidate.selected)
                            .map(|candidate| candidate.source_index)
                            .collect();
                        let issues = preview
                            .issues
                            .iter()
                            .map(|issue| issue.message.clone())
                            .collect();
                        view.snippet_import_review = Some(SnippetImportReview {
                            #[cfg(feature = "right-rail-acceptance")]
                            file_name,
                            candidates,
                            issues,
                            phase: SnippetImportPhase::Reviewing,
                            status: None,
                            preview,
                            selected_source_indices,
                            batch_request: None,
                            batch_id: None,
                        });
                        view.status = None;
                        cx.emit(super::SnippetEditorVisibilityChanged {
                            open: view.snippet_overlay_open(),
                            focus_command_draft: false,
                        });
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 成功后关闭预览并切到我的片段；未知结果保留批次身份，禁止重复提交。
    ///
    /// @author kongweiguang
    fn apply_snippet_import_result(
        &mut self,
        result: Result<SnippetImportBatchState, AppError>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_transfer_generation != generation {
            return;
        }
        let mut imported_count = None;
        {
            let Some(review) = self.snippet_import_review.as_mut() else {
                return;
            };
            match result {
                Ok(SnippetImportBatchState::Committed(receipt)) => {
                    review.phase = SnippetImportPhase::Committed;
                    review.batch_request = None;
                    review.batch_id = None;
                    review.preview = SnippetImportPreview::default();
                    review.selected_source_indices.clear();
                    for row in &mut review.candidates {
                        row.selected = false;
                    }
                    review.status = Some(transfer_message(
                        format!("已导入 {} 个片段。", receipt.imported_ids.len()),
                        format!("Imported {} snippets.", receipt.imported_ids.len()),
                    ));
                    imported_count = Some(receipt.imported_ids.len());
                }
                Ok(SnippetImportBatchState::NotCommitted) => {
                    review.phase = SnippetImportPhase::NotCommitted;
                    review.status = Some(transfer_message(
                        "尚未提交；可显式重试同一批次，或调整选择后建立新批次。",
                        "The batch was not committed. You can explicitly retry it or change the selection to create a new batch.",
                    ));
                }
                Ok(SnippetImportBatchState::OutcomeUnknown) => {
                    review.phase = SnippetImportPhase::OutcomeUnknown;
                    review.status = Some(transfer_message(
                        "提交结果未知；只能查询批次状态，不能重新提交。",
                        "The commit result is unknown. Query this batch; do not resubmit it.",
                    ));
                }
                Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                    review.phase = SnippetImportPhase::OutcomeUnknown;
                    review.status = Some(transfer_message(
                        "提交结果未知；只能查询批次状态，不能重新提交。",
                        "The commit result is unknown. Query this batch; do not resubmit it.",
                    ));
                }
                Err(error) => {
                    review.phase = SnippetImportPhase::NotCommitted;
                    review.status = Some(error.public_message().to_owned());
                }
            }
        }
        if let Some(count) = imported_count {
            self.snippet_import_review = None;
            self.snippet_catalog_section =
                kerminal_application::commands::SnippetCatalogSection::Mine;
            self.refresh_snippet_page(cx);
            self.status = Some(transfer_message(
                format!("已导入 {count} 个片段。"),
                format!("Imported {count} snippets."),
            ));
            cx.emit(super::SnippetEditorVisibilityChanged {
                open: self.snippet_overlay_open(),
                focus_command_draft: false,
            });
        }
        cx.notify();
    }

    /// 新文件操作替换旧 UI 请求并推进代次；已提交的应用任务仍由 executor 跟踪。
    ///
    /// @author kongweiguang
    fn begin_snippet_transfer(&mut self) -> (u64, CancellationToken) {
        if let Some(previous) = self.snippet_transfer_cancellation.take() {
            previous.cancel();
        }
        self.snippet_transfer_generation = self.snippet_transfer_generation.wrapping_add(1).max(1);
        let generation = self.snippet_transfer_generation;
        let cancellation = CancellationToken::new();
        self.snippet_transfer_cancellation = Some(cancellation.clone());
        self.snippet_transfer_busy = true;
        (generation, cancellation)
    }

    /// 只释放当前操作代次，防止迟到的 picker/future 清除新操作的 busy 状态。
    ///
    /// @author kongweiguang
    fn finish_snippet_transfer(&mut self, generation: u64) -> bool {
        if self.snippet_transfer_generation != generation {
            return false;
        }
        self.snippet_transfer_busy = false;
        self.snippet_transfer_cancellation = None;
        true
    }

    /// validator 与目录启动共用一个代次，使关闭管理层能取消并隔离迟到结果。
    ///
    /// @author kongweiguang
    fn begin_snippet_manager_operation(&mut self) -> (u64, CancellationToken) {
        if let Some(previous) = self.snippet_manager_cancellation.take() {
            previous.cancel();
        }
        self.snippet_manager_generation = self.snippet_manager_generation.wrapping_add(1).max(1);
        let generation = self.snippet_manager_generation;
        let cancellation = CancellationToken::new();
        self.snippet_manager_cancellation = Some(cancellation.clone());
        (generation, cancellation)
    }

    /// 仅当前 manager 操作可释放它自己的 token 槽。
    ///
    /// @author kongweiguang
    fn finish_snippet_manager_operation(&mut self, generation: u64) -> bool {
        if self.snippet_manager_generation != generation {
            return false;
        }
        self.snippet_manager_cancellation = None;
        true
    }
}

/// 统一使用当前 Kit locale；不把绝对路径、命令正文或变量值拼入状态消息。
/// UI 状态统一使用当前 locale，并避免把路径、命令正文或变量值拼入说明。
///
/// @author kongweiguang
fn transfer_message(
    zh_cn: impl Into<gpui_kit::SharedString>,
    en_us: impl Into<gpui_kit::SharedString>,
) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}
