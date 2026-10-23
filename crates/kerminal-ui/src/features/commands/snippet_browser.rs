//! 片段目录查询与弹层生命周期；所有读写仍由 CommandsService 和其应用用例完成。
//!
//! @author kongweiguang

use gpui_kit::{ClipboardItem, Context, IntoElement, Window};
use kerminal_application::{
    AppError, PublicErrorCode,
    commands::{
        SnippetCatalogCursor, SnippetCatalogPageOutcome, SnippetCatalogPageRequest,
        SnippetCatalogRevision, SnippetCatalogSection, SnippetExecutionCatalogSnapshot,
        VersionedCommandDefinition,
    },
};
use kerminal_domain::commands::{
    SnippetCatalogOrigin, SnippetDefaultAction, SnippetDefinition, SnippetPreferenceKey,
    SnippetRisk, SnippetScope, SnippetVariableKind,
};
use kerminal_domain::preferences::ConfigRevision;
use tokio_util::sync::CancellationToken;

use super::{
    CommandsSection, CommandsView, SnippetEditorVisibilityChanged, command_error_context,
    command_message, snippet_definition_from_catalog,
};

const SNIPPET_PAGE_SIZE: usize = 100;

/// 只接管目录局部的 `/` 与 Ctrl/Cmd+F；修饰键组合不会被误当成普通搜索入口。
///
/// @author kongweiguang
pub(super) fn is_snippet_search_shortcut(
    key: &str,
    control: bool,
    platform: bool,
    alt: bool,
    shift: bool,
) -> bool {
    if alt || shift {
        return false;
    }
    let command_modifier = control || platform;
    (key == "/" && !command_modifier) || (key.eq_ignore_ascii_case("f") && command_modifier)
}

/// 复制后台读回前同时核对目录 revision 与展示定义，避免把更新后的命令伪装成旧行内容。
///
/// @author kongweiguang
fn snippet_copy_snapshot_matches(
    snapshot: &SnippetExecutionCatalogSnapshot,
    key: &SnippetPreferenceKey,
    definition: &SnippetDefinition,
    expected_revision: Option<&ConfigRevision>,
) -> bool {
    if &snapshot.key != key || &snapshot.definition != definition {
        return false;
    }
    match (&snapshot.revision, expected_revision, key.origin) {
        (SnippetCatalogRevision::User(actual), Some(expected), SnippetCatalogOrigin::User) => {
            actual == expected
        }
        (SnippetCatalogRevision::Builtin { .. }, None, SnippetCatalogOrigin::Builtin) => true,
        _ => false,
    }
}

/// 保存结果未知时固定来源身份和新定义正文，后续只能核对该候选。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct PendingBuiltinSnippetCopy {
    source_key: SnippetPreferenceKey,
    expected_definition: SnippetDefinition,
}

/// 列表展开只记录精确来源与加载反馈；定义、变量输入和运行计划继续由 CommandsView 唯一持有。
///
/// @author kongweiguang
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum SnippetInlinePreview {
    #[default]
    Collapsed,
    Loading(SnippetPreferenceKey),
    Ready(SnippetPreferenceKey),
    Failed {
        key: SnippetPreferenceKey,
        message: String,
    },
}

/// 标题动作只切换内联投影、复用当前详情，或精确读取另一个目录项。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnippetPreviewToggleAction {
    Collapse,
    RevealSelected,
    LoadExact,
}

impl SnippetInlinePreview {
    /// 查询切换或加载失败后，旧条目仍不能借用新条目的详情数据。
    pub(super) fn matches_key(&self, key: &SnippetPreferenceKey) -> bool {
        match self {
            Self::Collapsed => false,
            Self::Loading(current) | Self::Ready(current) => current == key,
            Self::Failed { key: current, .. } => current == key,
        }
    }

    /// 同一片段只改展开状态；运行时变量和定义草稿仍属于当前实体，不进入离开门禁。
    ///
    /// @author kongweiguang
    fn toggle_action(
        &self,
        key: &SnippetPreferenceKey,
        selected_key_matches: bool,
    ) -> SnippetPreviewToggleAction {
        if self.matches_key(key) {
            SnippetPreviewToggleAction::Collapse
        } else if selected_key_matches {
            SnippetPreviewToggleAction::RevealSelected
        } else {
            SnippetPreviewToggleAction::LoadExact
        }
    }

    /// 异步预览只在仍等待同一来源键时接收；收起或切走后旧回执不能安装旧详情。
    ///
    /// @author kongweiguang
    fn is_loading_key(&self, key: &SnippetPreferenceKey) -> bool {
        matches!(self, Self::Loading(current) if current == key)
    }

    /// 成功加载必须来自当前精确键，折叠或切换后的迟到回执不重新展开预览。
    ///
    /// @author kongweiguang
    fn finish_loading(&mut self, key: &SnippetPreferenceKey, result: Result<(), String>) -> bool {
        if !matches!(self, Self::Loading(current) if current == key) {
            return false;
        }
        *self = match result {
            Ok(()) => Self::Ready(key.clone()),
            Err(message) => Self::Failed {
                key: key.clone(),
                message,
            },
        };
        true
    }

    /// 成功的精确读取更新选中详情；若当前展开项已切换，则隐藏不匹配的表单投影。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_loaded_key(&mut self, loaded_key: &SnippetPreferenceKey) {
        match self {
            Self::Collapsed => {}
            Self::Loading(key) if key == loaded_key => {
                *self = Self::Ready(loaded_key.clone());
            }
            Self::Ready(key) | Self::Failed { key, .. } if key != loaded_key => {
                *self = Self::Collapsed;
            }
            Self::Loading(_) | Self::Ready(_) | Self::Failed { .. } => {}
        }
    }
}

/// 一次目录请求的不可变票据；旧回执必须同时匹配查询与页面代次。
#[derive(Clone)]
struct SnippetPageRequestTicket {
    page_generation: u64,
    query_generation: u64,
    cancellation: CancellationToken,
}

/// 为替换查询或 continuation 建立请求票据；替换会取消旧请求，加载更多则拒绝并发。
///
/// @author kongweiguang
fn begin_snippet_page_request(
    page_generation: &mut u64,
    query_generation: u64,
    loading: &mut bool,
    active_cancellation: &mut Option<CancellationToken>,
    append: bool,
) -> Option<SnippetPageRequestTicket> {
    if append && *loading {
        return None;
    }
    if let Some(previous) = active_cancellation.take() {
        previous.cancel();
    }
    *loading = false;
    *page_generation = page_generation.wrapping_add(1).max(1);
    let cancellation = CancellationToken::new();
    *active_cancellation = Some(cancellation.clone());
    *loading = true;
    Some(SnippetPageRequestTicket {
        page_generation: *page_generation,
        query_generation,
        cancellation,
    })
}

/// 旧页回执不能清除新请求的 loading 或替换其结果。
///
/// @author kongweiguang
fn is_current_snippet_page_request(
    ticket: &SnippetPageRequestTicket,
    page_generation: u64,
    query_generation: u64,
) -> bool {
    ticket.page_generation == page_generation
        && ticket.query_generation == query_generation
        && !ticket.cancellation.is_cancelled()
}

/// 完成回执仅释放与自身代次匹配的请求槽。
///
/// @author kongweiguang
fn finish_snippet_page_request(
    ticket: &SnippetPageRequestTicket,
    page_generation: u64,
    query_generation: u64,
    loading: &mut bool,
    active_cancellation: &mut Option<CancellationToken>,
) -> bool {
    if !is_current_snippet_page_request(ticket, page_generation, query_generation) {
        return false;
    }
    *loading = false;
    *active_cancellation = None;
    true
}

/// 只有当前筛选与最近成功页一致时，才允许继续分页或复用旧游标。
///
/// @author kongweiguang
pub(super) fn snippet_page_matches_filter(
    loaded_section: Option<SnippetCatalogSection>,
    loaded_query: Option<&str>,
    section: SnippetCatalogSection,
    query: &str,
) -> bool {
    loaded_section == Some(section) && loaded_query == Some(query)
}

/// 查询未成功覆盖当前筛选时，保留的旧行必须标成不可操作的过期投影。
///
/// @author kongweiguang
pub(super) fn snippet_page_is_stale(
    loaded_section: Option<SnippetCatalogSection>,
    loaded_query: Option<&str>,
    section: SnippetCatalogSection,
    query: &str,
    has_error: bool,
    retrying_continuation: bool,
) -> bool {
    !snippet_page_matches_filter(loaded_section, loaded_query, section, query)
        || (has_error && !retrying_continuation)
}

/// 仅成功查询后的空页能显示“无匹配”；请求中或失败时必须展示加载/错误状态。
///
/// @author kongweiguang
pub(super) fn should_show_snippet_empty_state(
    item_count: usize,
    loading: bool,
    has_error: bool,
    loaded_filter_matches: bool,
) -> bool {
    item_count == 0 && !loading && !has_error && loaded_filter_matches
}

/// 新成功页整体替换或追加，并一次性更新游标、筛选身份与错误投影。
///
/// @author kongweiguang
/// 一次成功页回执需要更新的列表投影；把同一视图状态放在一起避免长参数链。
///
/// @author kongweiguang
struct SnippetPageSuccessState<'a, T> {
    items: &'a mut Vec<T>,
    total_count: &'a mut usize,
    next_cursor: &'a mut Option<SnippetCatalogCursor>,
    loaded_section: &'a mut Option<SnippetCatalogSection>,
    loaded_query: &'a mut Option<String>,
    page_error: &'a mut Option<String>,
    retry_cursor: &'a mut Option<SnippetCatalogCursor>,
}

/// 页面值与发起请求时的过滤身份一起保存，迟到页面不会借用当前筛选条件。
///
/// @author kongweiguang
struct SnippetPageSuccess<T> {
    items: Vec<T>,
    total: usize,
    next_cursor: Option<SnippetCatalogCursor>,
    section: SnippetCatalogSection,
    query: String,
    append: bool,
}

/// 根据原请求身份应用成功页，续页追加、第一页替换，并只清理该页反馈状态。
///
/// @author kongweiguang
fn apply_snippet_page_success<T>(
    state: SnippetPageSuccessState<'_, T>,
    page: SnippetPageSuccess<T>,
) {
    if page.append {
        state.items.extend(page.items);
    } else {
        *state.items = page.items;
    }
    *state.total_count = page.total;
    *state.next_cursor = page.next_cursor;
    *state.loaded_section = Some(page.section);
    *state.loaded_query = Some(page.query);
    *state.page_error = None;
    *state.retry_cursor = None;
}

/// 失败只更新错误与精确重试位置，已成功展示的目录页继续保留。
///
/// @author kongweiguang
fn record_snippet_page_failure(
    page_error: &mut Option<String>,
    retry_cursor: &mut Option<SnippetCatalogCursor>,
    message: String,
    failed_cursor: Option<SnippetCatalogCursor>,
) {
    *page_error = Some(message);
    *retry_cursor = failed_cursor;
}

/// 收藏由 origin+id 定位，避免同名内置与用户片段产生跨来源写入。
impl CommandsView {
    /// 配置变更或导入完成时统一重置游标并替换当前查询，防止旧页回调覆盖新目录。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_snippet_page(&mut self, cx: &mut Context<Self>) {
        self.snippet_inline_preview = SnippetInlinePreview::Collapsed;
        self.snippet_query_generation = self.snippet_query_generation.wrapping_add(1).max(1);
        self.snippet_next_cursor = None;
        self.snippet_page_retry_cursor = None;
        self.snippet_page_error = None;
        self.load_snippet_page(None, cx);
    }

    /// 输入变化会取消在途页请求并从新过滤条件的第一页开始，迟到结果由代次丢弃。
    ///
    /// @author kongweiguang
    pub(super) fn schedule_snippet_query(&mut self, query: String, cx: &mut Context<Self>) {
        let current_query = self
            .snippet_query
            .update(cx, |input, _| input.value().to_string());
        if current_query != query {
            return;
        }
        self.snippet_inline_preview = SnippetInlinePreview::Collapsed;
        self.snippet_query_generation = self.snippet_query_generation.wrapping_add(1).max(1);
        self.snippet_next_cursor = None;
        self.snippet_page_retry_cursor = None;
        self.snippet_page_error = None;
        self.load_snippet_page(None, cx);
    }

    /// 用实体保存的输入状态更新搜索；订阅负责触发唯一查询路径，避免重复请求。
    ///
    /// @author kongweiguang
    pub fn set_snippet_query(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.snippet_query
            .update(cx, |input, cx| input.set_value(query, window, cx));
    }

    /// 切换目录区段时清除 continuation；应用层会对非空 query 使用全目录搜索。
    ///
    /// @author kongweiguang
    pub fn select_snippet_catalog_section(
        &mut self,
        section: SnippetCatalogSection,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_catalog_section == section {
            return;
        }
        self.snippet_inline_preview = SnippetInlinePreview::Collapsed;
        self.snippet_query_generation = self.snippet_query_generation.wrapping_add(1).max(1);
        self.snippet_catalog_section = section;
        self.snippet_next_cursor = None;
        self.snippet_page_retry_cursor = None;
        self.snippet_page_error = None;
        self.load_snippet_page(None, cx);
    }

    /// 请求第一页或 continuation，并在请求边界重新确认代次和取消令牌。
    ///
    /// @author kongweiguang
    pub(super) fn load_snippet_page(
        &mut self,
        cursor: Option<SnippetCatalogCursor>,
        cx: &mut Context<Self>,
    ) {
        let append = cursor.is_some();
        // continuation 只能串行加载；第一页则必须替换旧过滤请求，不能被 loading 锁吞掉。
        let Some(ticket) = begin_snippet_page_request(
            &mut self.snippet_page_generation,
            self.snippet_query_generation,
            &mut self.snippet_page_loading,
            &mut self.snippet_page_cancellation,
            append,
        ) else {
            return;
        };
        let request_cancel = ticket.cancellation.clone();
        let query = self
            .snippet_query
            .update(cx, |input, _| input.value().to_string());
        let request_section = self.snippet_catalog_section;
        let request_query = query.clone();
        let request = SnippetCatalogPageRequest {
            section: request_section,
            query: (!query.trim().is_empty()).then_some(query),
            scope: None,
            page_size: SNIPPET_PAGE_SIZE,
            cursor: cursor.clone(),
        };
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        let completion_ticket = ticket.clone();
        let failed_cursor = cursor.clone();

        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request_cancel.clone(), move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "片段目录查询已取消",
                            "Snippet catalog query was cancelled",
                        )));
                    }
                    commands.list_snippet_page(request).await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !is_current_snippet_page_request(
                    &completion_ticket,
                    view.snippet_page_generation,
                    view.snippet_query_generation,
                ) || !finish_snippet_page_request(
                    &completion_ticket,
                    view.snippet_page_generation,
                    view.snippet_query_generation,
                    &mut view.snippet_page_loading,
                    &mut view.snippet_page_cancellation,
                ) {
                    return;
                }
                match result {
                    Ok(SnippetCatalogPageOutcome::Page(page)) => {
                        apply_snippet_page_success(
                            SnippetPageSuccessState {
                                items: &mut view.snippet_page_items,
                                total_count: &mut view.snippet_total_count,
                                next_cursor: &mut view.snippet_next_cursor,
                                loaded_section: &mut view.snippet_page_loaded_section,
                                loaded_query: &mut view.snippet_page_loaded_query,
                                page_error: &mut view.snippet_page_error,
                                retry_cursor: &mut view.snippet_page_retry_cursor,
                            },
                            SnippetPageSuccess {
                                items: page.items,
                                total: page.total,
                                next_cursor: page.next_cursor,
                                section: request_section,
                                query: request_query,
                                append,
                            },
                        );
                        view.status = None;
                    }
                    Ok(SnippetCatalogPageOutcome::StaleCursor) => {
                        view.snippet_next_cursor = None;
                        view.snippet_page_retry_cursor = None;
                        let message = command_message(
                            "片段目录已变化，正在重新读取第一页",
                            "The snippet catalog changed; reloading the first page",
                        );
                        record_snippet_page_failure(
                            &mut view.snippet_page_error,
                            &mut view.snippet_page_retry_cursor,
                            message.clone(),
                            None,
                        );
                        view.status = Some(message);
                        if append {
                            view.load_snippet_page(None, cx);
                        }
                    }
                    Err(error) => {
                        let message = error.public_message().to_owned();
                        record_snippet_page_failure(
                            &mut view.snippet_page_error,
                            &mut view.snippet_page_retry_cursor,
                            message.clone(),
                            failed_cursor,
                        );
                        view.status = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 仅在当前页仍有 continuation 时追加；重复点击不会并行读取同一页。
    ///
    /// @author kongweiguang
    pub fn load_more_snippets(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_page_loading || self.snippet_page_error.is_some() {
            return;
        }
        let query = self
            .snippet_query
            .update(cx, |input, _| input.value().to_string());
        if !snippet_page_matches_filter(
            self.snippet_page_loaded_section,
            self.snippet_page_loaded_query.as_deref(),
            self.snippet_catalog_section,
            &query,
        ) {
            return;
        }
        let Some(cursor) = self.snippet_next_cursor.clone() else {
            return;
        };
        self.load_snippet_page(Some(cursor), cx);
    }

    /// 重试失败查询；若失败的是 continuation，只重试该游标，不清除已成功的前页。
    ///
    /// @author kongweiguang
    pub fn retry_snippet_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_page_loading {
            return;
        }
        let query = self
            .snippet_query
            .update(cx, |input, _| input.value().to_string());
        let page_matches = snippet_page_matches_filter(
            self.snippet_page_loaded_section,
            self.snippet_page_loaded_query.as_deref(),
            self.snippet_catalog_section,
            &query,
        );
        let retry_cursor = if page_matches {
            self.snippet_page_retry_cursor.take()
        } else {
            None
        };
        if retry_cursor.is_none() {
            self.snippet_next_cursor = None;
        }
        self.snippet_page_error = None;
        self.status = None;
        self.load_snippet_page(retry_cursor, cx);
    }

    /// 复制已显示的精确定义模板；读回时复核 revision，避免把后台新版本当成当前行内容。
    ///
    /// @author kongweiguang
    pub(super) fn copy_snippet(&mut self, key: SnippetPreferenceKey, cx: &mut Context<Self>) {
        let Some(selected) = self.selected_snippet.as_ref() else {
            self.status = Some(command_message(
                "请先展开片段详情，再复制命令模板",
                "Expand a snippet detail before copying its command template",
            ));
            cx.notify();
            return;
        };
        let selected_origin = if selected.revision.is_some() {
            SnippetCatalogOrigin::User
        } else {
            SnippetCatalogOrigin::Builtin
        };
        if selected.definition.id != key.snippet_id || selected_origin != key.origin {
            self.status = Some(command_message(
                "片段来源已变化，未复制命令模板",
                "The snippet source changed; no command template was copied",
            ));
            cx.notify();
            return;
        }
        let expected_definition = selected.definition.clone();
        let expected_revision = selected.revision.clone();
        let (generation, cancellation) = self.begin_snippet_copy();
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        self.status = Some(command_message(
            "正在复制片段模板…",
            "Copying snippet template…",
        ));
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation.clone(), move |operation_cancel| async move {
                    let snapshot = commands
                        .get_snippet_execution_snapshot(key.clone())
                        .await?
                        .ok_or_else(|| {
                            AppError::not_found(command_error_context(
                                "指定来源的片段不存在",
                                "The snippet from that source no longer exists",
                            ))
                        })?;
                    if !snippet_copy_snapshot_matches(
                        &snapshot,
                        &key,
                        &expected_definition,
                        expected_revision.as_ref(),
                    ) {
                        return Err(AppError::conflict(command_error_context(
                            "片段内容或 revision 已变化，请重新展开后复制",
                            "The snippet content or revision changed; reopen it before copying",
                        )));
                    }
                    if operation_cancel.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "片段复制已取消",
                            "Snippet copy was cancelled",
                        )));
                    }
                    Ok(snapshot.definition.command)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.snippet_copy_generation != generation || cancellation.is_cancelled() {
                    return;
                }
                view.snippet_copy_cancellation = None;
                match result {
                    Ok(template) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(template));
                        view.status = Some(command_message(
                            "片段模板已交给系统剪贴板",
                            "Snippet template was sent to the system clipboard",
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

    /// 复制操作保留模板语义，不为剪贴板生成或缓存一次运行预览。
    ///
    /// @author kongweiguang
    pub(super) fn copy_snippet_inline_preview(
        &mut self,
        key: SnippetPreferenceKey,
        cx: &mut Context<Self>,
    ) {
        self.copy_snippet(key, cx);
    }

    /// 复制只展开当前表单中的命令与变量；剪贴板是明确用户动作，不依赖终端平台或 shell 查询。
    ///
    /// @author kongweiguang
    pub(super) fn copy_snippet_preview(
        &mut self,
        key: SnippetPreferenceKey,
        cx: &mut Context<Self>,
    ) {
        let selected_matches = self.selected_snippet.as_ref().is_some_and(|selected| {
            !self.snippet_is_new
                && selected.definition.id == key.snippet_id
                && selected.revision.is_some() == (key.origin == SnippetCatalogOrigin::User)
        });
        if !selected_matches {
            self.status = Some(command_message(
                "片段详情已变化，未复制命令",
                "The selected snippet changed; no command was copied",
            ));
            cx.notify();
            return;
        }
        let Some(rendered) = super::snippet_fill::render_selected_snippet_direct(self, cx) else {
            self.status = Some(command_message("请先选择片段", "Select a snippet first"));
            cx.notify();
            return;
        };
        if rendered.command.trim().is_empty() {
            self.status = Some(command_message(
                "命令片段内容为空",
                "Snippet command is empty",
            ));
            cx.notify();
            return;
        }
        let status = if rendered.sensitive {
            command_message(
                "敏感命令已复制；不会写入命令历史",
                "Sensitive command copied; it will not be recorded in command history",
            )
        } else {
            command_message("命令已复制", "Command copied")
        };
        cx.write_to_clipboard(ClipboardItem::new_string(rendered.command));
        self.status = Some(status);
        cx.notify();
    }
    /// 将当前只读内置定义另存为新的用户定义；创建 CAS 与精确读回避免覆盖或盲目重试。
    ///
    /// @author kongweiguang
    pub(super) fn save_builtin_snippet_as_user(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_saving
            || self.confirm_snippet_delete
            || self.pending_snippet_transition.is_some()
        {
            self.status = Some(command_message(
                "请先完成当前片段操作，再保存到我的",
                "Finish the current snippet operation before saving a copy",
            ));
            cx.notify();
            return;
        }
        let Some(selected) = self.selected_snippet.as_ref() else {
            self.status = Some(command_message(
                "请先打开内置片段详情",
                "Open a built-in snippet before saving a copy",
            ));
            cx.notify();
            return;
        };
        if self.snippet_is_new || selected.revision.is_some() {
            self.status = Some(command_message(
                "只有内置片段可以另存到我的",
                "Only a built-in snippet can be saved as a user copy",
            ));
            cx.notify();
            return;
        }
        let source = selected.definition.clone();
        let source_key = SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::Builtin,
            snippet_id: source.id.clone(),
        };
        if let Some(pending) = self.pending_builtin_copy.clone() {
            if !pending_builtin_copy_matches_source(&pending, &source_key) {
                self.status = Some(command_message(
                    "另一个内置片段的保存结果仍待核对；请先打开原片段再核对，不会重新创建",
                    "A copy of another built-in snippet still needs reconciliation; reopen that source to check it. No new copy will be created",
                ));
                cx.notify();
                return;
            }
            self.reconcile_builtin_snippet_copy(pending, window, cx);
            return;
        }
        let values = match self.collect_snippet_draft_values(cx) {
            Ok(values) => values,
            Err(message) => {
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        let new_id = self.commands.new_definition_id();
        let definition = copy_builtin_definition_to_user(&source, values, new_id);
        let pending_copy = PendingBuiltinSnippetCopy {
            source_key,
            expected_definition: definition,
        };
        let expected_definition = pending_copy.expected_definition.clone();
        let service = self.commands.clone();
        let readback_service = service.clone();
        let executor = self.executor.clone();
        cancel_snippet_copy(self);
        self.snippet_generation = self.snippet_generation.wrapping_add(1).max(1);
        let generation = self.snippet_generation;
        self.pending_builtin_copy = Some(pending_copy);
        self.snippet_saving = true;
        self.status = Some(command_message(
            "正在将内置片段另存到我的…",
            "Saving a user copy of the built-in snippet…",
        ));
        cx.spawn_in(window, async move |this, cx| {
            let save_definition = expected_definition.clone();
            let save_result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "片段另存已取消",
                            "Saving the snippet copy was cancelled",
                        )));
                    }
                    service.save_snippet_versioned(save_definition, None).await
                })
                .await;
            // executor 收到任务 panic 或端口返回未知时也只能读回固定 ID，不能重发创建。
            let result = if let Err(error) = save_result {
                if error.public_code() == PublicErrorCode::OutcomeUnknown {
                    let readback_expected = expected_definition.clone();
                    match executor
                        .run(move |cancellation| async move {
                            if cancellation.is_cancelled() {
                                return Err(AppError::cancelled(command_error_context(
                                    "片段另存结果核对已取消",
                                    "Checking the snippet copy result was cancelled",
                                )));
                            }
                            let saved = readback_service
                                .get_user_snippet_versioned(readback_expected.id.clone())
                                .await?;
                            match saved {
                                Some(saved)
                                    if is_exact_user_snippet_copy(&readback_expected, &saved) =>
                                {
                                    Ok(saved)
                                }
                                _ => Err(AppError::outcome_unknown(command_error_context(
                                    "片段另存结果仍无法确认；保留原 ID 待再次核对",
                                    "The snippet copy result is still unknown; its original ID is retained for another check",
                                ))),
                            }
                        })
                        .await
                    {
                        Ok(saved) => Ok(saved),
                        Err(_) => Err(error),
                    }
                } else {
                    Err(error)
                }
            } else {
                save_result
            };
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_builtin_snippet_copy(generation, result, window, cx);
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 只读取保存回执未知时预留的 ID；该分支永不再次调用创建端口。
    ///
    /// @author kongweiguang
    fn reconcile_builtin_snippet_copy(
        &mut self,
        pending: PendingBuiltinSnippetCopy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let expected = pending.expected_definition.clone();
        self.snippet_generation = self.snippet_generation.wrapping_add(1).max(1);
        let generation = self.snippet_generation;
        self.snippet_saving = true;
        self.status = Some(command_message(
            "正在核对原片段副本的固定 ID…",
            "Checking the original snippet copy ID…",
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "片段另存结果核对已取消",
                            "Checking the snippet copy result was cancelled",
                        )));
                    }
                    let saved = service
                        .get_user_snippet_versioned(expected.id.clone())
                        .await?;
                    match saved {
                        Some(saved) if is_exact_user_snippet_copy(&expected, &saved) => Ok(saved),
                        _ => Err(AppError::outcome_unknown(command_error_context(
                            "尚未读到完全匹配的用户副本；原 ID 保留，不会重新创建",
                            "No exact user copy was found yet; the original ID is retained and will not be recreated",
                        ))),
                    }
                })
                .await;
            let _ = cx.update(|_, app| {
                let _ = this.update(app, |view, cx| {
                    if view.snippet_generation != generation {
                        return;
                    }
                    view.snippet_saving = false;
                    match result {
                        Ok(_) => {
                            view.pending_builtin_copy = None;
                            view.status = Some(command_message(
                                "已确认用户副本存在；当前内置详情和运行变量保持不变",
                                "The user copy is confirmed. The current built-in detail and runtime variables were kept",
                            ));
                            view.reload_panel(cx);
                        }
                        Err(error) => {
                            // 读回失败或暂未出现都不能证明原提交失败，因此继续保留 ID。
                            view.status = Some(error.public_message().to_owned());
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 创建结果必须是同 ID、同正文且带用户文件 revision，才可解除未知状态。
    ///
    /// @author kongweiguang
    fn finish_builtin_snippet_copy(
        &mut self,
        generation: u64,
        result: Result<VersionedCommandDefinition<SnippetDefinition>, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_generation != generation {
            return;
        }
        match result {
            Ok(saved)
                if self.pending_builtin_copy.as_ref().is_some_and(|pending| {
                    is_exact_user_snippet_copy(&pending.expected_definition, &saved)
                }) =>
            {
                self.pending_builtin_copy = None;
                self.finish_snippet_save(generation, Ok(saved), window, cx);
            }
            Ok(_) => {
                self.snippet_saving = false;
                self.status = Some(command_message(
                    "保存回执与预留的副本不一致；原 ID 已保留待核对",
                    "The save receipt does not match the reserved copy; its original ID is retained for reconciliation",
                ));
                cx.notify();
            }
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                self.snippet_saving = false;
                self.status = Some(command_message(
                    "另存结果未知。再次点击“查询保存结果”只核对原 ID，不会重新创建",
                    "The copy result is unknown. Clicking “Check save result” again only checks the original ID and will not create another copy",
                ));
                cx.notify();
            }
            Err(error) => {
                self.pending_builtin_copy = None;
                self.snippet_saving = false;
                self.status = Some(error.public_message().to_owned());
                cx.notify();
            }
        }
    }

    /// 供渲染层把未知回执动作标成“核对”，避免用户误以为会创建第二个副本。
    ///
    /// @author kongweiguang
    pub(super) fn builtin_copy_requires_reconciliation(&self) -> bool {
        self.pending_builtin_copy.is_some()
    }

    /// 替换复制请求时取消旧任务，防止迟到结果覆盖更近的用户选择。
    ///
    /// @author kongweiguang
    fn begin_snippet_copy(&mut self) -> (u64, CancellationToken) {
        cancel_snippet_copy(self);
        let cancellation = CancellationToken::new();
        self.snippet_copy_cancellation = Some(cancellation.clone());
        (self.snippet_copy_generation, cancellation)
    }

    /// 收藏动作使用应用层 origin+ID 身份校验，失败时不乐观伪造收藏状态。
    ///
    /// @author kongweiguang
    pub fn set_snippet_favorite(
        &mut self,
        key: SnippetPreferenceKey,
        favorite: bool,
        cx: &mut Context<Self>,
    ) {
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        let generation = self.snippet_page_generation;
        let query_generation = self.snippet_query_generation;
        self.status = Some(command_message("正在保存收藏…", "Saving favorite…"));
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let preference = commands.set_snippet_favorite(key, favorite).await?;
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "片段收藏更新已取消",
                            "Snippet favorite update was cancelled",
                        )));
                    }
                    Ok(preference)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.snippet_page_generation != generation
                    || view.snippet_query_generation != query_generation
                {
                    return;
                }
                match result {
                    Ok(_) => {
                        view.status = None;
                        view.snippet_next_cursor = None;
                        view.snippet_page_retry_cursor = None;
                        view.snippet_page_error = None;
                        view.load_snippet_page(None, cx);
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 精确按来源打开详情；脏稿门禁保留完整 key 并重新显示编辑器，让关闭弹层后的决策仍可见。
    ///
    /// @author kongweiguang
    pub fn open_snippet_key(
        &mut self,
        key: SnippetPreferenceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_saving || self.confirm_snippet_delete {
            self.status = Some(command_message(
                "片段编辑操作尚未完成，当前无法切换",
                "Finish the current snippet operation before switching",
            ));
            cx.notify();
            return;
        }
        cancel_snippet_copy(self);
        self.show_compact_detail(CommandsSection::Snippets, cx);
        if self.selected_snippet.as_ref().is_some_and(|selected| {
            let origin = if selected.revision.is_some() {
                SnippetCatalogOrigin::User
            } else {
                SnippetCatalogOrigin::Builtin
            };
            selected.definition.id == key.snippet_id && origin == key.origin
        }) {
            self.set_snippet_editor_open(true, cx);
            return;
        }
        if self.snippet_draft_is_dirty(cx) {
            self.pending_snippet_transition = Some(super::EditorTransition::OpenKey(key));
            self.set_snippet_editor_open(true, cx);
            self.status = Some(command_message(
                "片段草稿尚未保存，请选择保存、丢弃或取消切换",
                "The snippet draft has unsaved changes. Save, discard, or cancel the switch",
            ));
            cx.notify();
            return;
        }
        self.load_snippet_key(key, true, false, window, cx);
    }

    /// 标题用于展开列表内预览；编辑表单仍是显式动作，脏草稿沿用原有保存/放弃/取消门禁。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_snippet_preview(
        &mut self,
        key: SnippetPreferenceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_saving || self.confirm_snippet_delete {
            self.status = Some(command_message(
                "片段编辑操作尚未完成，当前无法切换",
                "Finish the current snippet operation before switching",
            ));
            cx.notify();
            return;
        }

        let selected_key_matches = self.selected_snippet_matches_key(&key);
        match self
            .snippet_inline_preview
            .toggle_action(&key, selected_key_matches)
        {
            SnippetPreviewToggleAction::Collapse => {
                self.snippet_inline_preview = SnippetInlinePreview::Collapsed;
                cx.notify();
                return;
            }
            SnippetPreviewToggleAction::RevealSelected => {
                self.snippet_inline_preview = SnippetInlinePreview::Ready(key);
                self.status = None;
                cx.notify();
                return;
            }
            SnippetPreviewToggleAction::LoadExact => {}
        }

        if self.snippet_draft_is_dirty(cx) {
            self.pending_snippet_transition = Some(super::EditorTransition::PreviewKey(key));
            self.set_snippet_editor_open(true, cx);
            self.status = Some(command_message(
                "片段草稿尚未保存，请选择保存、丢弃或取消切换",
                "The snippet draft has unsaved changes. Save, discard, or cancel the switch",
            ));
            cx.notify();
            return;
        }

        self.snippet_inline_preview = SnippetInlinePreview::Loading(key.clone());
        self.status = None;
        self.load_snippet_key(key, false, true, window, cx);
    }

    /// 失败反馈只重试原来的精确来源键，不把当前选中项或其它来源当作替代结果。
    ///
    /// @author kongweiguang
    pub(super) fn retry_snippet_preview(
        &mut self,
        key: SnippetPreferenceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(
            &self.snippet_inline_preview,
            SnippetInlinePreview::Failed { key: failed, .. } if failed == &key
        ) {
            return;
        }
        if self.snippet_draft_is_dirty(cx) {
            self.pending_snippet_transition = Some(super::EditorTransition::PreviewKey(key));
            self.set_snippet_editor_open(true, cx);
            self.status = Some(command_message(
                "片段草稿尚未保存，请选择保存、丢弃或取消切换",
                "The snippet draft has unsaved changes. Save, discard, or cancel the switch",
            ));
            cx.notify();
            return;
        }
        self.snippet_inline_preview = SnippetInlinePreview::Loading(key.clone());
        self.load_snippet_key(key, false, true, window, cx);
    }

    /// Inline action 必须仍指向同一 origin+id，不能因目录同名或加载结果改变选中目标。
    ///
    /// @author kongweiguang
    pub(super) fn selected_snippet_matches_key(&self, key: &SnippetPreferenceKey) -> bool {
        self.selected_snippet.as_ref().is_some_and(|selected| {
            selected.definition.id == key.snippet_id
                && (selected.revision.is_some() == (key.origin == SnippetCatalogOrigin::User))
        })
    }

    /// 加载 exact catalog identity；内联预览只在仍为该键的 Loading 时接收回执，避免折叠后迟到内容覆盖选择。
    /// 用户项继续用文件 revision，内置项保持只读。
    ///
    /// @author kongweiguang
    pub(super) fn load_snippet_key(
        &mut self,
        key: SnippetPreferenceKey,
        open_after_load: bool,
        preview_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        let completion_key = key.clone();
        self.snippet_generation = self.snippet_generation.wrapping_add(1).max(1);
        let generation = self.snippet_generation;
        self.status = Some(command_message("正在读取片段…", "Loading snippet…"));
        cx.spawn_in(window, async move |this, cx| {
            let loaded = executor
                .run(move |cancel| async move {
                    let item = commands
                        .get_snippet_catalog_item(key.clone())
                        .await?
                        .ok_or_else(|| {
                            AppError::not_found(command_error_context(
                                "指定来源的片段不存在",
                                "The snippet from that source no longer exists",
                            ))
                        })?;
                    if item.origin != key.origin || item.id != key.snippet_id {
                        return Err(AppError::not_found(command_error_context(
                            "片段目录身份已变化",
                            "The snippet catalog identity changed",
                        )));
                    }
                    let definition = if key.origin == SnippetCatalogOrigin::User {
                        commands
                            .get_user_snippet_versioned(key.snippet_id.clone())
                            .await?
                            .ok_or_else(|| {
                                AppError::not_found(command_error_context(
                                    "用户片段定义不存在",
                                    "User snippet definition no longer exists",
                                ))
                            })?
                    } else {
                        VersionedCommandDefinition {
                            definition: snippet_definition_from_catalog(item.clone()),
                            revision: None,
                        }
                    };
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "片段读取已取消",
                            "Snippet loading was cancelled",
                        )));
                    }
                    Ok((definition, item.sensitive))
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    if view.snippet_generation != generation {
                        return;
                    }
                    if preview_only && !view.snippet_inline_preview.is_loading_key(&completion_key) {
                        return;
                    }
                    match loaded {
                        Ok((definition, sensitive)) => {
                            if !open_after_load
                                && view.selected_snippet.is_some()
                                && !view.snippet_draft_matches_saved(cx)
                            {
                                let conflict = command_message(
                                    "刷新期间片段草稿发生变化；本地内容已保留。",
                                    "The snippet draft changed during refresh; the local draft was preserved.",
                                );
                                view.configuration_conflict = Some(conflict.clone());
                                view.status = Some(conflict);
                                if view.snippet_inline_preview.matches_key(&completion_key) {
                                    view.snippet_inline_preview = SnippetInlinePreview::Collapsed;
                                }
                                cx.notify();
                                return;
                            }
                            view.install_snippet(definition, window, cx);
                            view.selected_snippet_sensitive = sensitive;
                            view.show_compact_detail(CommandsSection::Snippets, cx);
                            if open_after_load {
                                view.set_snippet_editor_open(true, cx);
                            }
                        }
                        Err(error) => {
                            let message = error.public_message().to_owned();
                            view.snippet_inline_preview
                                .finish_loading(&completion_key, Err(message.clone()));
                            view.status = Some(message);
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 根级弹层只报告可见性变化，并推进独立代次供异步 Fill 拒绝关闭后来重开的编辑器。
    /// 草稿、revision 与输入实体始终留在同一个 CommandsView 中。
    ///
    /// @author kongweiguang
    pub(super) fn set_snippet_editor_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.set_snippet_editor_open_with_focus(open, false, cx);
    }

    /// 可见性事件携带 Fill 专用焦点意图，避免模态关闭后焦点仍停在隐藏的变量输入框。
    ///
    /// @author kongweiguang
    fn set_snippet_editor_open_with_focus(
        &mut self,
        open: bool,
        focus_command_draft: bool,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_editor_open == open {
            return;
        }
        self.snippet_editor_open = open;
        self.snippet_editor_generation = self.snippet_editor_generation.wrapping_add(1).max(1);
        cx.emit(SnippetEditorVisibilityChanged {
            open,
            focus_command_draft,
        });
        cx.notify();
    }

    /// 读取同一实体中的弹层开关，不从 Root 保存第二份编辑状态。
    ///
    /// @author kongweiguang
    pub fn snippet_editor_open(&self) -> bool {
        self.snippet_editor_open
    }

    /// 根工作台只挂这一份已存在的编辑表单，不另建输入实体或 modal 草稿。
    ///
    /// @author kongweiguang
    pub(crate) fn render_snippet_editor_modal(
        &mut self,
        theme: crate::theme::ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_snippet_dialog_content(theme, window, cx)
    }

    /// 弹层打开后一次性聚焦标题输入；不在每次重绘时抢回终端焦点。
    ///
    /// @author kongweiguang
    pub fn focus_snippet_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editable_title = self.snippet_is_new
            || self
                .selected_snippet
                .as_ref()
                .is_some_and(|snippet| snippet.revision.is_some());
        if editable_title {
            self.snippet_title
                .update(cx, |input, cx| input.focus(window, cx));
        } else if let Some(variable) = self.snippet_variables.first() {
            variable
                .input
                .update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Fill 后只在历史区已选中且编辑器已关闭时聚焦现有命令输入实体。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_command_draft(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.section != CommandsSection::History || self.snippet_editor_open {
            return false;
        }
        self.command_input
            .update(cx, |input, cx| input.focus(window, cx));
        true
    }

    /// Escape 与关闭按钮使用相同门禁；dirty 草稿保留在实体中供稍后继续。
    ///
    /// @author kongweiguang
    pub fn handle_snippet_editor_escape(&mut self, cx: &mut Context<Self>) {
        self.close_snippet_editor(cx);
    }

    /// 只隐藏编辑器；保存、删除确认和切换决策未完成时不允许绕过既有状态机。
    ///
    /// @author kongweiguang
    pub fn close_snippet_editor(&mut self, cx: &mut Context<Self>) {
        self.close_snippet_editor_with_focus(false, cx);
    }

    /// Fill 关闭弹层沿用同一保存/删除/切换门禁，但把焦点返回意图交给历史命令输入框。
    ///
    /// @author kongweiguang
    pub(super) fn close_snippet_editor_for_fill(&mut self, cx: &mut Context<Self>) {
        self.close_snippet_editor_with_focus(true, cx);
    }

    /// 普通关闭恢复原焦点，只有 Fill 请求才把焦点意图改投到命令草稿。
    ///
    /// @author kongweiguang
    fn close_snippet_editor_with_focus(
        &mut self,
        focus_command_draft: bool,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_saving
            || self.confirm_snippet_delete
            || self.pending_snippet_transition.is_some()
        {
            self.status = Some(command_message(
                "请先完成当前片段确认，再关闭编辑器",
                "Finish the pending snippet decision before closing the editor",
            ));
            cx.notify();
            return;
        }
        self.set_snippet_editor_open_with_focus(false, focus_command_draft, cx);
    }

    /// modal 子视图仅委托 C 的同一表单 renderer，避免工具栏槽与根弹层双挂输入实体。
    ///
    /// @author kongweiguang
    pub fn handle_snippet_editor_modal_escape(&mut self, cx: &mut Context<Self>) {
        self.handle_snippet_editor_escape(cx);
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/commands_snippet_browser.rs"]
mod tests;

/// 生成用户副本时沿用当前完整定义和草稿元数据，但绝不持久化敏感变量默认值。
///
/// @author kongweiguang
fn copy_builtin_definition_to_user(
    source: &SnippetDefinition,
    values: super::SnippetDraftValues,
    new_id: String,
) -> SnippetDefinition {
    let mut definition = source.clone();
    definition.id = new_id;
    definition.title = values.title;
    definition.command = values.command;
    definition.description = values.description;
    definition.tags = values.tags;
    definition.sort_order = values.sort_order;
    definition.scope = values.scope;
    definition.category = values.category;
    definition.risk = values.risk;
    definition.default_action = values.default_action;
    definition.context_bindings = values.context_bindings;
    definition.variables = values
        .variables
        .into_iter()
        .map(|mut variable| {
            if variable.sensitive || variable.kind == SnippetVariableKind::Secret {
                variable.sensitive = true;
                variable.kind = SnippetVariableKind::Secret;
                variable.default_value = None;
                variable.suggestions.clear();
            }
            variable
        })
        .collect();
    definition.derived_from = Some(source.id.clone());
    definition
}

/// 未知回执只能由原 builtin 来源触发核对，不能把待确认 ID 套到其它条目。
///
/// @author kongweiguang
fn pending_builtin_copy_matches_source(
    pending: &PendingBuiltinSnippetCopy,
    source_key: &SnippetPreferenceKey,
) -> bool {
    &pending.source_key == source_key
}

/// 只把带用户文件 revision 的精确正文读回视为确认成功。
///
/// @author kongweiguang
fn is_exact_user_snippet_copy(
    expected: &SnippetDefinition,
    saved: &VersionedCommandDefinition<SnippetDefinition>,
) -> bool {
    saved.revision.is_some() && &saved.definition == expected
}

/// 面板释放时协作取消唯一目录页请求；应用 executor 仍负责跟踪后台任务直到完成。
pub(super) fn cancel_snippet_page_request(cancellation: &mut Option<CancellationToken>) {
    if let Some(cancellation) = cancellation.take() {
        cancellation.cancel();
    }
}

/// 取消已启动的复制，并推进代次，使已排队的剪贴板写入失效。
///
/// @author kongweiguang
pub(super) fn cancel_snippet_copy(view: &mut CommandsView) {
    view.snippet_copy_generation = view.snippet_copy_generation.wrapping_add(1).max(1);
    if let Some(cancellation) = view.snippet_copy_cancellation.take() {
        cancellation.cancel();
    }
}

impl gpui_kit::EventEmitter<SnippetEditorVisibilityChanged> for CommandsView {}

impl CommandsView {
    /// 通过显式选项设置用户定义字段，避免 Tab/Dropdown 的选择状态与循环按钮耦合。
    ///
    /// @author kongweiguang
    pub fn set_snippet_scope(&mut self, scope: SnippetScope, cx: &mut Context<Self>) {
        if self.snippet_scope != scope {
            self.snippet_scope = scope;
            cx.notify();
        }
    }

    /// 风险等级直接反映用户选择；未知等级仍可保存并要求 Run 用例确认。
    ///
    /// @author kongweiguang
    pub fn set_snippet_risk(&mut self, risk: SnippetRisk, cx: &mut Context<Self>) {
        if self.snippet_risk != risk {
            self.snippet_risk = risk;
            cx.notify();
        }
    }

    /// 默认动作是保存的定义元数据，更新它不会自动执行片段。
    ///
    /// @author kongweiguang
    pub fn set_snippet_default_action(
        &mut self,
        action: SnippetDefaultAction,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_default_action != action {
            self.snippet_default_action = action;
            cx.notify();
        }
    }
}
