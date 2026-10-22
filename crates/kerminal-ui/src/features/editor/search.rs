//! 编辑器查找与替换的后台协调；重扫描、定位和替换规划均不得阻塞 GPUI 线程。
//!
//! @author kongweiguang

use std::{ops::Range, sync::Arc, time::Duration};

use gpui_kit::{
    AppContext as _, Context, Entity, HighlightStyle, Hsla, Window,
    component::input::{EditorState, TextDecoration},
    rgb,
};
use kerminal_application::AppError;
use kerminal_domain::files::FileEndpoint;
use tokio_util::sync::CancellationToken;

use super::search_model::{
    EditorSearchIndex, EditorSearchMatcher, EditorSearchOption, SearchWorkError,
};
use super::workspace::{EditorStatus, FileEditorWorkspace};
use crate::theme::ThemeTokens;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(70);

/// 普通命中与当前命中分别采用主题警示色和强调色，样式直接作用于匹配字形。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SearchDecorationStyles {
    normal: HighlightStyle,
    active: HighlightStyle,
}

impl SearchDecorationStyles {
    /// 从同一套浅深主题令牌生成双色文本样式，保证颜色变更不会重扫文档。
    pub(super) fn from_theme(theme: ThemeTokens) -> Self {
        let normal_background = theme.warning;
        let active_background = theme.accent;
        Self {
            normal: HighlightStyle {
                color: Some(highlight_foreground(normal_background)),
                background_color: Some(normal_background),
                ..Default::default()
            },
            active: HighlightStyle {
                color: Some(highlight_foreground(active_background)),
                background_color: Some(active_background),
                ..Default::default()
            },
        }
    }
}

/// 按令牌背景亮度选择黑白文字，避免普通或活动命中在浅深主题下失去对比。
fn highlight_foreground(background: Hsla) -> Hsla {
    if background.l >= 0.55 {
        rgb(0x18181b).into()
    } else {
        rgb(0xffffff).into()
    }
}

/// 搜索栏反馈当前唯一后台工作，避免把扫描或替换计划伪装成已完成。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SearchActivity {
    #[default]
    Idle,
    Scanning,
    Locating,
    Replacing,
}

/// 搜索票据固定文件、读取、正文和查询代次，异步回执不得跨代次回写。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SearchTicket {
    endpoint: FileEndpoint,
    load_generation: u64,
    document_generation: u64,
    search_generation: u64,
}

/// 活动文档结果只保留一次正文快照、精确总数和有界稀疏索引。
#[derive(Clone)]
pub(super) struct EditorSearchResults {
    ticket: SearchTicket,
    text: Arc<str>,
    matcher: EditorSearchMatcher,
    index: Arc<EditorSearchIndex>,
}

impl EditorSearchResults {
    /// UI 只读取精确计数，不暴露稀疏索引内部或把高亮数当成总命中数。
    pub(super) fn total_matches(&self) -> usize {
        self.index.total_matches()
    }
}

/// 扫描结果预先解析所需活动命中，防止 UI 回调中重扫大型文档。
struct SearchScanOutput {
    text: Arc<str>,
    matcher: EditorSearchMatcher,
    index: EditorSearchIndex,
    selected_index: Option<usize>,
    selected_range: Option<Range<usize>>,
}

/// 当前项替换回执的完整 UI 快照，代次、匹配序号和编辑器必须作为同一结果复核。
///
/// @author kongweiguang
struct ReplaceCurrentCompletion {
    ticket: SearchTicket,
    operation_generation: u64,
    selected_index: usize,
    editor: Entity<EditorState>,
    result: Result<Result<Option<String>, SearchWorkError>, AppError>,
}

/// 全量替换回执同时绑定版本票据、编辑器和目标端点，防止迟到结果写入另一份文档。
///
/// @author kongweiguang
struct ReplaceAllCompletion {
    ticket: SearchTicket,
    operation_generation: u64,
    editor: Entity<EditorState>,
    endpoint: FileEndpoint,
    result: Result<Result<(String, usize), SearchWorkError>, AppError>,
}

impl FileEditorWorkspace {
    /// 主题变化只刷新现有有界装饰；搜索索引和异步任务仍由原代次管理。
    pub(super) fn update_search_decoration_theme(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) {
        let styles = SearchDecorationStyles::from_theme(theme);
        if self.search_decoration_styles == styles {
            return;
        }
        self.search_decoration_styles = styles;
        self.apply_search_decorations(cx);
    }

    /// 输入或活动文件变化时只启动带代次的后台扫描，GPUI 线程不物化正文字符串。
    pub(super) fn sync_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        self.start_search(true, cx);
        cx.notify();
    }

    /// 从输入实体读取查询，避免标签切换消费尚未同步到工作区字段的旧值。
    pub(super) fn current_search_query(&self, cx: &mut Context<Self>) -> String {
        self.find_input
            .update(cx, |state, _| state.value().to_string())
    }

    /// 切换选项后重新编译同一规则，使查找、导航和替换保持一致语义。
    pub(super) fn toggle_search_option(
        &mut self,
        option: EditorSearchOption,
        cx: &mut Context<Self>,
    ) {
        match option {
            EditorSearchOption::MatchCase => {
                self.search_options.match_case = !self.search_options.match_case
            }
            EditorSearchOption::WholeWord => {
                self.search_options.whole_word = !self.search_options.whole_word
            }
            EditorSearchOption::Regex => self.search_options.regex = !self.search_options.regex,
        }
        let query = self
            .find_input
            .update(cx, |state, _| state.value().to_string());
        self.sync_search_query(query, cx);
    }

    /// 编辑内容变更提升该文档代次，后台旧扫描失效后在新快照上重算而不移动光标。
    pub(super) fn refresh_search_document(
        &mut self,
        endpoint: &FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.endpoint == endpoint) else {
            return;
        };
        tab.document_generation = tab.document_generation.wrapping_add(1).max(1);
        if self.active_endpoint.as_ref() == Some(endpoint) && self.search_visible() {
            self.start_search(false, cx);
        }
        cx.notify();
    }

    /// 关闭查找时取消所有工作并清除装饰，避免迟到回执恢复已关闭的结果。
    pub(super) fn clear_search_projection(&mut self, cx: &mut Context<Self>) {
        self.search_generation = next_generation(self.search_generation);
        self.cancel_search_operation();
        self.search_results = None;
        self.search_current_range = None;
        self.search_match_index = None;
        self.search_error = None;
        self.clear_search_decorations(cx);
    }

    /// 在精确总数上循环导航，超出首批高亮时通过后台稀疏索引按需定位。
    ///
    /// @author kongweiguang
    pub(super) fn move_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.search_activity != SearchActivity::Idle {
            return;
        }
        let Some(results) = self.search_results.clone() else {
            return;
        };
        let count = results.index.total_matches();
        if count == 0 {
            return;
        }
        let target_index = match self.search_match_index {
            None if forward => 0,
            None => count - 1,
            Some(index) if forward => (index + 1) % count,
            Some(0) => count - 1,
            Some(index) => index - 1,
        };
        self.search_match_index = Some(target_index);
        if let Some(range) = results.index.highlights().get(target_index).cloned() {
            self.apply_search_range(&results.ticket, target_index, range, cx);
            return;
        }

        let (operation_generation, cancellation) =
            self.begin_search_operation(SearchActivity::Locating);
        self.search_current_range = None;
        self.apply_search_decorations(cx);
        let matcher = results.matcher.clone();
        let text = results.text.clone();
        let index = results.index.clone();
        let ticket = results.ticket.clone();
        let executor = self.executor.clone();
        let background = cx.background_executor().clone();
        let window_handle = self.window_handle;
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |work_cancellation| async move {
                    let result = background
                        .spawn(async move {
                            matcher.resolve_range_at(
                                &text,
                                &index,
                                target_index,
                                &work_cancellation,
                            )
                        })
                        .await;
                    Ok::<_, AppError>(result)
                })
                .await;
            let _ = cx.update_window(window_handle, |_, _, app| {
                let _ = this.update(app, |workspace, cx| {
                    workspace.finish_search_navigation(
                        ticket,
                        operation_generation,
                        target_index,
                        result,
                        cx,
                    );
                });
            });
        });
        self.search_operation_task = Some(task);
        cx.notify();
    }

    /// 当前项替换在线程池计算，回到 UI 后再次验证文件代次、命中和只读状态再编辑。
    ///
    /// @author kongweiguang
    pub(super) fn replace_current(
        &mut self,
        replacement: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_replace_match() {
            return;
        }
        let Some(results) = self.search_results.clone() else {
            return;
        };
        let Some(range) = self.search_current_range.clone() else {
            return;
        };
        let Some(editor) = self.active_tab().and_then(|tab| tab.editor.clone()) else {
            return;
        };
        let selected_index = self.search_match_index.unwrap_or(0);
        let (operation_generation, cancellation) =
            self.begin_search_operation(SearchActivity::Replacing);
        let matcher = results.matcher.clone();
        let text = results.text.clone();
        let ticket = results.ticket.clone();
        let executor = self.executor.clone();
        let background = cx.background_executor().clone();
        let window_handle = self.window_handle;
        let replacement_for_worker = replacement;
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |work_cancellation| async move {
                    let result = background
                        .spawn(async move {
                            matcher.replace_current(
                                &text,
                                &range,
                                &replacement_for_worker,
                                &work_cancellation,
                            )
                        })
                        .await;
                    Ok::<_, AppError>(result)
                })
                .await;
            let _ = cx.update_window(window_handle, |_, window, app| {
                let _ = this.update(app, |workspace, cx| {
                    workspace.finish_replace_current(
                        ReplaceCurrentCompletion {
                            ticket,
                            operation_generation,
                            selected_index,
                            editor,
                            result,
                        },
                        window,
                        cx,
                    );
                });
            });
        });
        self.search_operation_task = Some(task);
        cx.notify();
    }

    /// 全部替换后台流式生成完整正文；UI 只在版本与权限复核后提交一笔 GPUI undo。
    ///
    /// @author kongweiguang
    pub(super) fn replace_all(
        &mut self,
        replacement: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_replace_match() {
            return;
        }
        let Some(results) = self.search_results.clone() else {
            return;
        };
        let Some(editor) = self.active_tab().and_then(|tab| tab.editor.clone()) else {
            return;
        };
        let (operation_generation, cancellation) =
            self.begin_search_operation(SearchActivity::Replacing);
        let matcher = results.matcher.clone();
        let text = results.text.clone();
        let ticket = results.ticket.clone();
        let endpoint = ticket.endpoint.clone();
        let executor = self.executor.clone();
        let background = cx.background_executor().clone();
        let window_handle = self.window_handle;
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |work_cancellation| async move {
                    let result = background
                        .spawn(async move {
                            matcher.replace_all(&text, &replacement, &work_cancellation)
                        })
                        .await;
                    Ok::<_, AppError>(result)
                })
                .await;
            let _ = cx.update_window(window_handle, |_, window, app| {
                let _ = this.update(app, |workspace, cx| {
                    workspace.finish_replace_all(
                        ReplaceAllCompletion {
                            ticket,
                            operation_generation,
                            editor,
                            endpoint,
                            result,
                        },
                        window,
                        cx,
                    );
                });
            });
        });
        self.search_operation_task = Some(task);
        cx.notify();
    }

    /// 启动新查询快照时先使旧代次失效，再把 Rope 副本和 matcher 移入受监督后台任务。
    ///
    /// @author kongweiguang
    fn start_search(&mut self, select_first: bool, cx: &mut Context<Self>) {
        let previous_index = self.search_match_index.unwrap_or(0);
        self.search_generation = next_generation(self.search_generation);
        self.cancel_search_operation();
        self.search_results = None;
        self.search_current_range = None;
        self.search_match_index = None;
        self.search_error = None;
        self.clear_search_decorations(cx);

        if !self.search_visible() || self.search_query.is_empty() {
            return;
        }
        let query = self.search_query.clone();
        let options = self.search_options;
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(editor) = tab.editor.clone() else {
            return;
        };
        let ticket = SearchTicket {
            endpoint: tab.endpoint.clone(),
            load_generation: tab.load_generation,
            document_generation: tab.document_generation,
            search_generation: self.search_generation,
        };
        let rope = editor.update(cx, |state, _| state.text().clone());
        let desired_index = if select_first { 0 } else { previous_index };
        let (operation_generation, cancellation) =
            self.begin_search_operation(SearchActivity::Scanning);
        let executor = self.executor.clone();
        let background = cx.background_executor().clone();
        let window_handle = self.window_handle;
        let task = cx.spawn(async move |this, cx| {
            background.timer(SEARCH_DEBOUNCE).await;
            let result = executor
                .run_cancellable(cancellation.clone(), move |work_cancellation| async move {
                    let result = background
                        .spawn(async move {
                            if work_cancellation.is_cancelled() {
                                return Err(SearchWorkError::Cancelled);
                            }
                            let matcher = EditorSearchMatcher::compile(&query, options)?
                                .ok_or(SearchWorkError::InvalidCapture)?;
                            if work_cancellation.is_cancelled() {
                                return Err(SearchWorkError::Cancelled);
                            }
                            let text: Arc<str> = Arc::from(rope.to_string());
                            let index = matcher.scan(&text, &work_cancellation)?;
                            let selected_index = (index.total_matches() > 0).then(|| {
                                desired_index.min(index.total_matches().saturating_sub(1))
                            });
                            let selected_range = match selected_index {
                                Some(match_index) => Some(
                                    matcher
                                        .resolve_range_at(
                                            &text,
                                            &index,
                                            match_index,
                                            &work_cancellation,
                                        )?
                                        .ok_or(SearchWorkError::InvalidCapture)?,
                                ),
                                None => None,
                            };
                            Ok::<_, SearchWorkError>(SearchScanOutput {
                                text,
                                matcher,
                                index,
                                selected_index,
                                selected_range,
                            })
                        })
                        .await;
                    Ok::<_, AppError>(result)
                })
                .await;
            let _ = cx.update_window(window_handle, |_, _, app| {
                let _ = this.update(app, |workspace, cx| {
                    workspace.finish_search_scan(
                        ticket,
                        editor,
                        operation_generation,
                        select_first,
                        result,
                        cx,
                    );
                });
            });
        });
        self.search_operation_task = Some(task);
    }

    /// 只接受仍对应活动标签、读取版本、正文版本和查询代次的扫描结果。
    fn finish_search_scan(
        &mut self,
        ticket: SearchTicket,
        editor: Entity<EditorState>,
        operation_generation: u64,
        select_first: bool,
        result: Result<Result<SearchScanOutput, SearchWorkError>, AppError>,
        cx: &mut Context<Self>,
    ) {
        if !self.search_ticket_is_current(&ticket)
            || self.search_operation_generation != operation_generation
        {
            return;
        }
        self.finish_search_operation();
        match result {
            Ok(Ok(output)) => {
                self.search_match_index = output.selected_index;
                self.search_current_range = output.selected_range.clone();
                self.search_results = Some(Arc::new(EditorSearchResults {
                    ticket,
                    text: output.text,
                    matcher: output.matcher,
                    index: Arc::new(output.index),
                }));
                self.apply_search_decorations(cx);
                if select_first && let Some(range) = output.selected_range {
                    editor.update(cx, |state, cx| state.set_selected_range(range, cx));
                }
            }
            Ok(Err(SearchWorkError::Cancelled)) | Err(AppError::Cancelled { .. }) => {}
            Ok(Err(SearchWorkError::InvalidCapture)) => {
                self.search_error = Some(
                    crate::locale::text(
                        "搜索结果无法定位，请修改查询后重试。",
                        "A search result could not be located. Change the query and try again.",
                    )
                    .to_string(),
                );
            }
            Ok(Err(SearchWorkError::InvalidPattern(error))) => {
                self.search_error = Some(error);
            }
            Ok(Err(
                error @ (SearchWorkError::ReplacementLimitExceeded
                | SearchWorkError::OutputAllocationFailed),
            )) => {
                self.search_error = Some(search_work_error_message(&error));
            }
            Err(error) => self.search_error = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 长距离导航只在结果票据与最新操作号均未变化时选中命中。
    fn finish_search_navigation(
        &mut self,
        ticket: SearchTicket,
        operation_generation: u64,
        target_index: usize,
        result: Result<Result<Option<Range<usize>>, SearchWorkError>, AppError>,
        cx: &mut Context<Self>,
    ) {
        if !self.search_ticket_is_current(&ticket)
            || self.search_operation_generation != operation_generation
        {
            return;
        }
        self.finish_search_operation();
        match result {
            Ok(Ok(Some(range))) => self.apply_search_range(&ticket, target_index, range, cx),
            Ok(Ok(None)) | Ok(Err(SearchWorkError::InvalidCapture)) => {
                self.search_error = Some(
                    crate::locale::text(
                        "搜索索引已失效，请重新输入查询。",
                        "The search index is no longer valid. Re-enter the query.",
                    )
                    .to_string(),
                );
                self.search_results = None;
                self.search_current_range = None;
                self.search_match_index = None;
                self.clear_search_decorations(cx);
            }
            Ok(Err(SearchWorkError::InvalidPattern(error))) => {
                self.search_error = Some(error);
            }
            Ok(Err(
                error @ (SearchWorkError::ReplacementLimitExceeded
                | SearchWorkError::OutputAllocationFailed),
            )) => {
                self.search_error = Some(search_work_error_message(&error));
            }
            Ok(Err(SearchWorkError::Cancelled)) | Err(AppError::Cancelled { .. }) => {}
            Err(error) => self.search_error = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 当前项替换回执必须仍指向同一匹配，GPUI replace 将实际编辑记录为一次 undo。
    ///
    /// @author kongweiguang
    fn finish_replace_current(
        &mut self,
        completion: ReplaceCurrentCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ReplaceCurrentCompletion {
            ticket,
            operation_generation,
            selected_index,
            editor,
            result,
        } = completion;
        if !self.search_ticket_is_current(&ticket)
            || self.search_operation_generation != operation_generation
        {
            return;
        }
        self.finish_search_operation();
        match result {
            Ok(Ok(Some(expanded))) => {
                if self.can_replace_active()
                    && self.search_match_index == Some(selected_index)
                    && let Some(range) = self.search_current_range.clone()
                {
                    editor.update(cx, |state, cx| {
                        state.set_selected_range(range, cx);
                        state.replace(expanded, window, cx);
                    });
                    self.refresh_dirty(&ticket.endpoint, cx);
                    self.status = Some(EditorStatus::Message(
                        crate::locale::text("已替换 1 处", "Replaced 1 occurrence").to_string(),
                    ));
                } else {
                    self.start_search(false, cx);
                }
            }
            Ok(Ok(None)) => self.start_search(false, cx),
            Ok(Err(SearchWorkError::Cancelled)) | Err(AppError::Cancelled { .. }) => {}
            Ok(Err(SearchWorkError::InvalidCapture)) => {
                self.search_error = Some(
                    crate::locale::text(
                        "当前匹配已变化，请重新查找。",
                        "The selected match changed. Search again before replacing.",
                    )
                    .to_string(),
                );
            }
            Ok(Err(SearchWorkError::InvalidPattern(error))) => {
                self.search_error = Some(error);
            }
            Ok(Err(
                error @ (SearchWorkError::ReplacementLimitExceeded
                | SearchWorkError::OutputAllocationFailed),
            )) => {
                self.search_error = Some(search_work_error_message(&error));
            }
            Err(error) => self.status = Some(EditorStatus::Message(error.to_string())),
        }
        cx.notify();
    }

    /// 全量替换只在原文档仍可编辑时应用完整计划，并让 GPUI 一次性登记 undo 记录。
    ///
    /// @author kongweiguang
    fn finish_replace_all(
        &mut self,
        completion: ReplaceAllCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ReplaceAllCompletion {
            ticket,
            operation_generation,
            editor,
            endpoint,
            result,
        } = completion;
        if !self.search_ticket_is_current(&ticket)
            || self.search_operation_generation != operation_generation
        {
            return;
        }
        self.finish_search_operation();
        match result {
            Ok(Ok((updated, count))) if self.can_replace_active() => {
                if count > 0 {
                    editor.update(cx, |state, cx| state.replace_all(updated, window, cx));
                    self.refresh_dirty(&endpoint, cx);
                }
                self.status = Some(EditorStatus::Message(
                    crate::locale::text(
                        format!("已替换 {count} 处"),
                        format!("Replaced {count} occurrence(s)"),
                    )
                    .to_string(),
                ));
            }
            Ok(Ok(_)) => self.start_search(false, cx),
            Ok(Err(SearchWorkError::Cancelled)) | Err(AppError::Cancelled { .. }) => {}
            Ok(Err(SearchWorkError::InvalidCapture)) => {
                self.search_error = Some(
                    crate::locale::text(
                        "替换计划无效，正文保持不变。",
                        "The replacement plan was invalid. The document was not changed.",
                    )
                    .to_string(),
                );
            }
            Ok(Err(SearchWorkError::InvalidPattern(error))) => {
                self.search_error = Some(error);
            }
            Ok(Err(
                error @ (SearchWorkError::ReplacementLimitExceeded
                | SearchWorkError::OutputAllocationFailed),
            )) => {
                self.search_error = Some(search_work_error_message(&error));
            }
            Err(error) => self.status = Some(EditorStatus::Message(error.to_string())),
        }
        cx.notify();
    }

    /// GPUI decoration 集合最多包含首批 256 项及当前活动项，避免高频查询持有无界范围表。
    ///
    /// @author kongweiguang
    fn apply_search_decorations(&self, cx: &mut Context<Self>) {
        let Some(results) = &self.search_results else {
            self.clear_search_decorations(cx);
            return;
        };
        let mut ranges = results.index.highlights().to_vec();
        if let Some(current) = &self.search_current_range
            && !ranges.contains(current)
        {
            if ranges.len() == 256 {
                ranges.pop();
            }
            ranges.push(current.clone());
        }
        ranges.sort_unstable_by_key(|range| (range.start, range.end));
        ranges.dedup();
        for tab in &self.tabs {
            if let Some(decorations) = &tab.search_decorations {
                if tab.endpoint == results.ticket.endpoint {
                    decorations.set(
                        search_decorations(
                            &ranges,
                            self.search_current_range.as_ref(),
                            self.search_decoration_styles,
                        ),
                        cx,
                    );
                } else {
                    decorations.set(Vec::new(), cx);
                }
            }
        }
    }

    /// 活动命中范围同时驱动装饰、编辑器选区及循环导航计数。
    fn apply_search_range(
        &mut self,
        ticket: &SearchTicket,
        target_index: usize,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if !self.search_ticket_is_current(ticket) {
            return;
        }
        self.cancel_search_operation();
        self.search_match_index = Some(target_index);
        self.search_current_range = Some(range.clone());
        self.search_activity = SearchActivity::Idle;
        self.search_operation_cancellation = None;
        self.search_operation_task = None;
        self.apply_search_decorations(cx);
        if let Some(editor) = self.active_tab().and_then(|tab| tab.editor.clone()) {
            editor.update(cx, |state, cx| state.set_selected_range(range, cx));
        }
        cx.notify();
    }

    /// 只把当前有效查询的正文 Rope 投影到后台前，核对活动标签身份和全部文档代次。
    fn search_ticket_is_current(&self, ticket: &SearchTicket) -> bool {
        self.search_visible()
            && self.search_generation == ticket.search_generation
            && self.active_endpoint.as_ref() == Some(&ticket.endpoint)
            && self.tabs.iter().any(|tab| {
                tab.endpoint == ticket.endpoint
                    && tab.load_generation == ticket.load_generation
                    && tab.document_generation == ticket.document_generation
                    && tab.editor.is_some()
            })
    }

    /// 新操作先取消旧 token 并丢弃 UI 等待句柄，应用监督器仍负责旧 future 的真实收尾。
    fn begin_search_operation(&mut self, activity: SearchActivity) -> (u64, CancellationToken) {
        self.cancel_search_operation();
        let generation = self.search_operation_generation;
        let cancellation = CancellationToken::new();
        self.search_operation_cancellation = Some(cancellation.clone());
        self.search_activity = activity;
        (generation, cancellation)
    }

    /// 取消协作任务但不伪造计算完成；迟到结果因 operation generation 不匹配而失效。
    fn cancel_search_operation(&mut self) {
        if let Some(cancellation) = self.search_operation_cancellation.take() {
            cancellation.cancel();
        }
        self.search_operation_task = None;
        self.search_operation_generation = next_generation(self.search_operation_generation);
        self.search_activity = SearchActivity::Idle;
    }

    /// 当前任务完成后释放 UI owner；ApplicationTaskExecutor 仍持有取消/等待期间的后台工作。
    fn finish_search_operation(&mut self) {
        self.search_operation_cancellation = None;
        self.search_operation_task = None;
        self.search_activity = SearchActivity::Idle;
    }

    /// 重建或关闭搜索时同步清空各 tab 装饰，避免换文档后遗留旧范围。
    fn clear_search_decorations(&self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let Some(decorations) = &tab.search_decorations {
                decorations.set(Vec::new(), cx);
            }
        }
    }

    /// 搜索替换与直接编辑共用领域脏状态比较，避免代码路径绕过保存基线。
    fn refresh_dirty(&mut self, endpoint: &FileEndpoint, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| &tab.endpoint == endpoint) else {
            return;
        };
        let content = self.tabs[index]
            .editor
            .as_ref()
            .map(|editor| editor.update(cx, |state, _| state.text().to_string()))
            .unwrap_or_default();
        self.update_dirty(endpoint, content, cx);
    }
}

/// 将后台替换安全边界映射为可见说明，避免超限或分配失败只留在任务日志中。
fn search_work_error_message(error: &SearchWorkError) -> String {
    let (zh, en) = match error {
        SearchWorkError::ReplacementLimitExceeded => (
            "替换结果超过编辑器 10 MiB 上限，正文未更改。",
            "The replacement would exceed the editor's 10 MiB limit. The document was not changed.",
        ),
        SearchWorkError::OutputAllocationFailed => (
            "无法为替换结果分配内存，正文未更改。",
            "Memory for the replacement result could not be allocated. The document was not changed.",
        ),
        _ => (
            "替换失败，正文未更改。",
            "Replacement failed. The document was not changed.",
        ),
    };
    crate::locale::text(zh, en).to_string()
}

/// 递增非零工作代次，避免回绕到“尚未启动”的初始值。
fn next_generation(generation: u64) -> u64 {
    generation.wrapping_add(1).max(1)
}

/// 高亮装饰只对有界命中集合建立 UTF-8 文本范围，不复制完整搜索结果到 UI。
fn search_decorations(
    matches: &[Range<usize>],
    active_match: Option<&Range<usize>>,
    styles: SearchDecorationStyles,
) -> Vec<TextDecoration> {
    matches
        .iter()
        .map(|range| {
            let style = if active_match.is_some_and(|active| active == range) {
                styles.active
            } else {
                styles.normal
            };
            TextDecoration::new(range.clone(), style)
        })
        .collect()
}

#[cfg(test)]
#[path = "../../../tests/unit/editor_search_decorations.rs"]
mod decoration_tests;
