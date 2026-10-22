//! 终端工具栏中的独立命令历史视图，不复用命令工作区的 Entity 状态。
//!
//! @author kongweiguang

use std::{collections::HashMap, sync::Arc};

use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, IntoElement, KeyDownEvent, Render, SharedString,
    Window,
    assets::IconName,
    component::input::{Input, InputState},
    component::{
        Disableable as _,
        button::{Button, ButtonVariants},
        menu::{DropdownMenu as _, PopupMenuItem},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{
    AppError, ApplicationTaskExecutor,
    commands::{CommandHistoryAvailability, CommandsService},
    preferences::ThemeMode,
    sessions::UiSessionBinding,
};
use kerminal_domain::{
    commands::{CommandHistoryEntry, CommandHistorySource, CommandHistoryTarget},
    identity::{HostId, PaneId},
    sessions::{SessionInfo, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;

use super::history_query::{HistoryScope, PAGE_SIZE, page_window};
use crate::theme::ThemeTokens;

const HISTORY_LIMIT: usize = 100;

/// Logs 当前绑定的 pane、目标和可信 host；session generation 只用于迟到响应保护。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogsTarget {
    binding: UiSessionBinding,
    scope: HistoryScope,
    target: CommandHistoryTarget,
}

impl LogsTarget {
    /// 将真实 UI binding 与会话快照匹配后冻结历史 scope；HostId 只接受组合根提供的可信引用。
    ///
    /// Telnet 与 Serial 的 SessionTarget 不包含 HostId，因此缺少配置映射时省略该筛选，
    /// 不从网络地址或设备路径推导主机身份。
    ///
    /// @author kongweiguang
    pub fn from_ui_session(
        binding: UiSessionBinding,
        session: &SessionInfo,
        remote_host_id: Option<HostId>,
    ) -> Option<Self> {
        if binding.session_id != session.session_id || binding.owner != session.owner {
            return None;
        }
        Self::from_ui_target(binding, &session.target, remote_host_id)
    }

    /// 从 Root pane 投影的强类型目标构造 scope，不要求伪造或补查完整 SessionInfo。
    ///
    /// SSH 的 HostId 必须与目标内的 HostId 一致；Telnet/Serial 没有 HostId 字段，
    /// 只接收调用方按该 pane 的权威保存目标解析出的可信 HostId，不使用当前选中主机。
    /// Local/Container 沿用旧 Logs 的目标范围，不继承无关的远端 HostId。
    ///
    /// @author kongweiguang
    pub fn from_ui_target(
        binding: UiSessionBinding,
        target: &SessionTarget,
        trusted_host: Option<HostId>,
    ) -> Option<Self> {
        let pane_id = match &binding.owner {
            SessionOwner::Ui { pane_id, .. } => pane_id.clone(),
            SessionOwner::Agent { .. } | SessionOwner::Mcp | SessionOwner::Service { .. } => {
                return None;
            }
        };
        if let SessionTarget::Ssh(ssh_target) = target
            && trusted_host
                .as_ref()
                .is_some_and(|host_id| host_id != &ssh_target.host_id)
        {
            return None;
        }
        Some(Self::from_session_parts(
            binding,
            target,
            pane_id,
            trusted_host,
        ))
    }

    /// 把会话目标映射为冻结 0.3.45 历史筛选，不把一次 session ID 写进 pane 级查询。
    ///
    /// @author kongweiguang
    fn from_session_parts(
        binding: UiSessionBinding,
        session_target: &SessionTarget,
        pane_id: PaneId,
        remote_host_id: Option<HostId>,
    ) -> Self {
        let (target, remote_host_id) = match session_target {
            SessionTarget::Local(_) => (CommandHistoryTarget::Local, None),
            SessionTarget::Ssh(target) => (
                CommandHistoryTarget::Ssh,
                remote_host_id.or_else(|| Some(target.host_id.clone())),
            ),
            SessionTarget::Telnet(_) => (CommandHistoryTarget::Telnet, remote_host_id),
            SessionTarget::Serial(_) => (CommandHistoryTarget::Serial, remote_host_id),
            SessionTarget::Container(_) => (CommandHistoryTarget::Container, None),
        };
        let scope = HistoryScope::pane(target, pane_id, remote_host_id);
        Self {
            binding,
            scope,
            target,
        }
    }

    /// 返回当前 pane 级请求范围，清理确认和搜索始终从同一 target 快照创建。
    ///
    /// @author kongweiguang
    fn scope(&self) -> &HistoryScope {
        &self.scope
    }

    /// 构造显示用协议名；不把 host endpoint 或完整连接配置暴露在列表标题中。
    ///
    /// @author kongweiguang
    fn label(&self) -> SharedString {
        match self.target {
            CommandHistoryTarget::Local => crate::locale::text("本地终端", "Local terminal"),
            CommandHistoryTarget::Ssh => crate::locale::text("SSH 终端", "SSH terminal"),
            CommandHistoryTarget::Telnet => crate::locale::text("Telnet 终端", "Telnet terminal"),
            CommandHistoryTarget::Serial => crate::locale::text("串口终端", "Serial terminal"),
            CommandHistoryTarget::Container => {
                crate::locale::text("容器终端", "Container terminal")
            }
        }
    }
}

/// 独立 Logs Entity；只持有筛选与展示状态，所有历史数据仍由共享 CommandsService 读取。
pub struct LogsView {
    commands: Arc<CommandsService>,
    executor: ApplicationTaskExecutor,
    query: Entity<InputState>,
    target: Option<LogsTarget>,
    /// 仅用于历史卡片的友好标签；请求和清理仍以 entry 中的 HostId 为准。
    host_display_names: HashMap<HostId, SharedString>,
    theme_mode: ThemeMode,
    active: bool,
    source: Option<CommandHistorySource>,
    page: usize,
    entries: Vec<CommandHistoryEntry>,
    loading: bool,
    error: Option<String>,
    status: Option<String>,
    confirm_clear: Option<LogsTarget>,
    generation: u64,
    read_cancellation: Option<CancellationToken>,
}

impl LogsView {
    /// 在 Root 单独创建的工具 Entity 中建立输入状态，避免与主 CommandsView 共用草稿或生命周期；占位提示说明回车即提交，保留窄栏空间给查询文本。
    ///
    /// @author kongweiguang
    pub fn new(
        commands: Arc<CommandsService>,
        executor: ApplicationTaskExecutor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索命令，回车确认",
                "Search commands · Enter",
            ))
        });
        Self {
            commands,
            executor,
            query,
            target: None,
            host_display_names: HashMap::new(),
            theme_mode: ThemeMode::System,
            active: false,
            source: None,
            page: 1,
            entries: Vec::new(),
            loading: false,
            error: None,
            status: None,
            confirm_clear: None,
            generation: 0,
            read_cancellation: None,
        }
    }

    /// 同步 Root 已加载的主机名投影，仅改变历史上下文的可读标签，不触碰冻结筛选目标。
    ///
    /// @author kongweiguang
    pub fn set_host_display_names(
        &mut self,
        names: HashMap<HostId, SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.host_display_names != names {
            self.host_display_names = names;
            cx.notify();
        }
    }

    /// 语言切换只刷新含回车提示的搜索占位，不重建输入实体或清空当前查询与焦点，避免增加挤占窄栏的提交按钮。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("搜索命令，回车确认", "Search commands · Enter"),
                window,
                cx,
            )
        });
        cx.notify();
    }

    /// pane/session generation 改变时取消旧查询并清空其行，防止迟到内容污染新目标。
    ///
    /// @author kongweiguang
    pub fn set_target(&mut self, target: Option<LogsTarget>, cx: &mut Context<Self>) {
        if self.target == target {
            return;
        }
        self.cancel_read();
        self.generation = self.generation.wrapping_add(1).max(1);
        self.target = target;
        self.entries.clear();
        self.page = 1;
        self.loading = false;
        self.error = None;
        self.status = None;
        self.confirm_clear = None;
        if self.active {
            self.reload_history(cx);
        }
        cx.notify();
    }

    /// 主题变化只触发重绘；颜色由 Window 当前主题令牌提供，不保留硬编码主题副本。
    ///
    /// @author kongweiguang
    pub fn set_theme_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        if self.theme_mode != mode {
            self.theme_mode = mode;
            cx.notify();
        }
    }

    /// 非活动工具停止读请求；重新活动时按现有筛选刷新，保留当前输入和来源选择。
    ///
    /// @author kongweiguang
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.reload_history(cx);
        } else {
            self.cancel_read();
            self.generation = self.generation.wrapping_add(1).max(1);
            self.loading = false;
            self.confirm_clear = None;
        }
        cx.notify();
    }

    /// 只查询明确 pane 目标的最近一百条；搜索、来源或目标变化会取消并淘汰旧请求。
    ///
    /// @author kongweiguang
    fn reload_history(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            self.cancel_read();
            self.entries.clear();
            self.loading = false;
            return;
        };
        if !self.active {
            return;
        }

        self.cancel_read();
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        let cancellation = CancellationToken::new();
        self.read_cancellation = Some(cancellation.clone());
        let request = target.scope().list_request(
            self.query.update(cx, |query, _| query.value().to_string()),
            self.source,
            HISTORY_LIMIT,
        );
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        self.loading = true;
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    if operation_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("命令历史查询已取消"));
                    }
                    let result = commands.list_history(request).await;
                    if operation_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("命令历史查询已取消"));
                    }
                    result
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.response_is_current(generation, &target) {
                    return;
                }
                view.loading = false;
                view.read_cancellation = None;
                match result {
                    Ok(entries) => {
                        view.entries = entries.into_iter().take(HISTORY_LIMIT).collect();
                        view.page = page_window(view.page, view.entries.len()).0;
                        view.status = None;
                    }
                    Err(error) => view.error = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 删除时从当前列表按 ID 重新取得行并校验 pane scope，避免渲染闭包持有整条记录或旧 binding。
    ///
    /// @author kongweiguang
    fn delete_entry(&mut self, requested_entry_id: &str, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let Some(entry_id) = self
            .entries
            .iter()
            .find(|entry| entry.id == requested_entry_id && target.scope().contains_entry(entry))
            .map(|entry| entry.id.clone())
        else {
            return;
        };
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        self.status =
            Some(crate::locale::text("正在删除历史记录…", "Deleting history entry…").to_string());
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("命令历史删除已取消"));
                    }
                    let result = commands.delete_history(entry_id).await;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("命令历史删除结果未确认"));
                    }
                    result
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.operation_target_is_current(&target) {
                    return;
                }
                match result {
                    Ok(true) => {
                        view.status = Some(
                            crate::locale::text("历史记录已删除", "History entry deleted")
                                .to_string(),
                        );
                        view.reload_history(cx);
                    }
                    Ok(false) => {
                        view.status = Some(
                            crate::locale::text(
                                "历史记录已不存在",
                                "History entry no longer exists",
                            )
                            .to_string(),
                        );
                        view.reload_history(cx);
                    }
                    Err(error) => view.error = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 第一次点击只冻结当前 scope 并显示确认；目标改变会撤销未确认的清理意图。
    ///
    /// @author kongweiguang
    fn request_clear_confirmation(&mut self, cx: &mut Context<Self>) {
        self.confirm_clear = self.target.clone();
        cx.notify();
    }

    /// 用户确认后按点击时保存的 pane/target/host 范围清理，不受后续搜索和焦点变化影响。
    ///
    /// @author kongweiguang
    fn confirm_clear_history(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.confirm_clear.take() else {
            return;
        };
        let scope = target.scope().clone();
        let request = scope.clear_request();
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        self.status =
            Some(crate::locale::text("正在清理历史记录…", "Clearing history…").to_string());
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("命令历史清理已取消"));
                    }
                    let result = commands.clear_history(request).await;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("命令历史清理结果未确认"));
                    }
                    result
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.operation_target_is_current(&target) {
                    return;
                }
                match result {
                    Ok(count) => {
                        view.status = Some(
                            crate::locale::text(
                                format!("已清理 {count} 条历史记录"),
                                format!("Cleared {count} history entries"),
                            )
                            .to_string(),
                        );
                        view.reload_history(cx);
                    }
                    Err(error) => view.error = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 改变来源时复位分页；旧请求会由 reload_history 协作取消并检查 generation。
    ///
    /// @author kongweiguang
    fn set_source(&mut self, source: Option<CommandHistorySource>, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        self.source = source;
        self.page = 1;
        self.reload_history(cx);
    }

    /// 一次清除搜索和来源条件再重载，避免两次独立更新间闪现旧筛选结果。
    ///
    /// @author kongweiguang
    fn clear_history_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |input, cx| {
            input.set_value(String::new(), window, cx);
        });
        self.source = None;
        self.page = 1;
        self.reload_history(cx);
        cx.notify();
    }

    /// 分页按钮只调整当前最多一百条查询结果的本地投影，不发起重复数据库请求。
    ///
    /// @author kongweiguang
    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let (page, _, _) = page_window(page, self.entries.len());
        if self.page != page {
            self.page = page;
            cx.notify();
        }
    }

    /// 查询结果只有活动、generation 和冻结 binding 全部匹配时才允许进入当前视图。
    ///
    /// @author kongweiguang
    fn response_is_current(&self, generation: u64, target: &LogsTarget) -> bool {
        response_matches_target(
            self.active,
            self.generation,
            generation,
            self.target.as_ref(),
            target,
        )
    }

    /// 写操作按已确认的原 scope 完成；同一 pane 搜索变化不能吞掉结果后的刷新。
    ///
    /// @author kongweiguang
    fn operation_target_is_current(&self, target: &LogsTarget) -> bool {
        operation_matches_target(self.active, self.target.as_ref(), target)
    }

    /// 新目标、停用或刷新时取消协作式查询；已确认写操作使用独立的捕获范围。
    ///
    /// @author kongweiguang
    fn cancel_read(&mut self) {
        if let Some(cancellation) = self.read_cancellation.take() {
            cancellation.cancel();
        }
    }
}

/// 迟到结果只有同时匹配活动状态、请求代次和完整 UI binding 才能更新视图。
///
/// @author kongweiguang
fn response_matches_target(
    active: bool,
    current_generation: u64,
    request_generation: u64,
    current_target: Option<&LogsTarget>,
    request_target: &LogsTarget,
) -> bool {
    active && current_generation == request_generation && current_target == Some(request_target)
}

/// 已确认的历史写操作只看 pane binding；同一 pane 的搜索代次变化不改变其写入范围。
///
/// @author kongweiguang
fn operation_matches_target(
    active: bool,
    current_target: Option<&LogsTarget>,
    request_target: &LogsTarget,
) -> bool {
    active && current_target == Some(request_target)
}

impl Render for LogsView {
    /// 窄栏把刷新/清理移到标题行，搜索只留输入和来源筛选；Enter 提交以保留可读输入宽度。
    /// 主机名只用于卡片可读性，HostId 留在悬浮与辅助标签；空态和全部操作仍保留冻结 pane scope 与原筛选语义。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let target = self.target.clone();
        let selected_source = self.source;
        let filters_active = self.query.update(cx, |query, _| {
            history_filters_active(&query.value(), selected_source)
        });
        let source_options = source_options();
        let selected_source_label = source_options
            .iter()
            .find(|(source, _)| *source == selected_source)
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| crate::locale::text("全部来源", "All sources"));
        let source_accessibility_label = crate::locale::text(
            format!("命令来源：{selected_source_label}"),
            format!("Command source: {selected_source_label}"),
        );
        let source_entity = cx.entity().clone();
        let source_filter = Button::new("logs-source-filter")
            .compact()
            .dropdown_caret(true)
            .when(selected_source.is_some(), |button| button.primary())
            .label(selected_source_label)
            .accessibility_label(source_accessibility_label.clone())
            .tooltip(source_accessibility_label)
            .dropdown_menu(move |mut menu, _, _| {
                for (source, label) in &source_options {
                    let source = *source;
                    let selected = source == selected_source;
                    let label = label.clone();
                    let entity = source_entity.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(selected).on_click(
                        move |_, _, app| {
                            entity.update(app, |view, cx| view.set_source(source, cx));
                        },
                    ));
                }
                menu
            });
        let available = matches!(
            self.commands.history_availability(),
            CommandHistoryAvailability::Available
        );
        let (current_page, start, end) = page_window(self.page, self.entries.len());
        let page_count = self.entries.len().div_ceil(PAGE_SIZE).max(1);
        let rows = self.entries[start..end].iter().map(|entry| {
            let entry_id = entry.id.clone();
            let command = entry.command.clone();
            let command_tooltip = command.clone();
            let context = history_entry_context(entry, target.as_ref(), &self.host_display_names);
            let context_tooltip =
                history_entry_context_detail(entry, target.as_ref(), &self.host_display_names);
            let context_accessibility = context_tooltip.clone();
            let created_at = entry.created_at.clone();
            let source = source_label(entry.source);
            let metadata_tooltip = format!("{source} · {created_at}");
            div()
                .id(format!("logs-history-row-{entry_id}"))
                .w_full()
                .p_2()
                .flex()
                .items_start()
                .gap_1()
                .rounded_md()
                .bg(theme.panel)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .id(format!("logs-history-command-{entry_id}"))
                                .w_full()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(theme.text_sm)
                                .font_family("monospace")
                                .text_color(theme.foreground)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(command_tooltip.clone()).build(window, cx)
                                })
                                .child(command),
                        )
                        .child(
                            div()
                                .id(format!("logs-history-metadata-{entry_id}"))
                                .w_full()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(metadata_tooltip.clone()).build(window, cx)
                                })
                                .child(format!("{source} · {created_at}")),
                        )
                        .child(
                            div()
                                .id(format!("logs-history-context-{entry_id}"))
                                .w_full()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .aria_label(context_accessibility)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(context_tooltip.clone()).build(window, cx)
                                })
                                .child(context),
                        ),
                )
                .child(
                    Button::new(format!("logs-history-delete-{entry_id}"))
                        .compact()
                        .label(crate::locale::text("删除", "Delete"))
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            view.delete_entry(&entry_id, cx);
                        })),
                )
        });
        let unbound = self.target.is_none();
        let error = self.error.clone();
        let status = self.status.clone();
        let loading = self.loading;
        let confirm_clear = self.confirm_clear.clone();
        let clear_enabled = self.target.is_some() && !self.entries.is_empty() && available;
        let show_empty_state = self.entries.is_empty() && !loading && error.is_none() && !unbound;
        let empty_state_message = if filters_active {
            crate::locale::text("没有符合条件的命令历史", "No matching command history")
        } else {
            crate::locale::text(
                "当前终端暂无命令历史",
                "No command history for this terminal",
            )
        };
        let mut empty_state_content = div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(empty_state(empty_state_message, theme));
        if show_empty_state && filters_active {
            let clear_filters_label = crate::locale::text("清除筛选", "Clear filters");
            empty_state_content = empty_state_content.child(
                Button::new("logs-history-clear-filters")
                    .compact()
                    .ghost()
                    .label(clear_filters_label.clone())
                    .accessibility_label(clear_filters_label)
                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                        view.clear_history_filters(window, cx);
                    })),
            );
        }
        let refresh_label = if loading {
            crate::locale::text("正在加载命令历史", "Loading command history")
        } else {
            crate::locale::text("刷新命令历史", "Refresh command history")
        };
        let mut content = div()
            .id("terminal-logs-view")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .overflow_hidden()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        div()
                            .min_w(px(0.))
                            .text_size(px(13.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(crate::locale::text("命令历史", "Command history")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("logs-refresh")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::RefreshCw)
                                    .accessibility_label(refresh_label.clone())
                                    .tooltip(refresh_label)
                                    .disabled(unbound || !available)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.reload_history(cx);
                                    })),
                            )
                            .child(
                                Button::new("logs-clear")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Delete)
                                    .accessibility_label(crate::locale::text(
                                        "清空命令历史",
                                        "Clear command history",
                                    ))
                                    .tooltip(crate::locale::text(
                                        "清空命令历史",
                                        "Clear command history",
                                    ))
                                    .disabled(!clear_enabled)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.request_clear_confirmation(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .on_key_down(cx.listener(|view, event: &KeyDownEvent, _, cx| {
                        let modifiers = event.keystroke.modifiers;
                        if event.keystroke.key.eq_ignore_ascii_case("enter")
                            && !modifiers.control
                            && !modifiers.alt
                            && !modifiers.platform
                            && !modifiers.shift
                        {
                            view.page = 1;
                            view.reload_history(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(Input::new(&self.query).flex_1().min_w(px(0.)))
                    .child(source_filter),
            )
            .child(
                div()
                    .id("logs-history-context")
                    .w_full()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(target.as_ref().map_or_else(
                        || crate::locale::text("未聚焦终端", "No focused terminal"),
                        LogsTarget::label,
                    )),
            )
            .child(
                div()
                    .id("logs-history-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows)
                    .when(show_empty_state, move |list| {
                        list.child(empty_state_content)
                    })
                    .when(unbound, |list| {
                        list.child(empty_state(
                            crate::locale::text(
                                "聚焦一个终端后查看该终端的命令历史",
                                "Focus a terminal to view its command history",
                            ),
                            theme,
                        ))
                    }),
            );

        if let Some(error) = error {
            content = content.child(
                div()
                    .id("logs-history-error")
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .bg(theme.panel)
                    .text_color(theme.error)
                    .text_size(theme.text_xs)
                    .child(error),
            );
        }
        if let Some(status) = status {
            content = content.child(
                div()
                    .id("logs-history-status")
                    .w_full()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(status),
            );
        }
        if let Some(target) = confirm_clear {
            content = content.child(
                div()
                    .id("logs-clear-confirmation")
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(theme.panel)
                    .child(crate::locale::text(
                        format!("确认清空{}的命令历史？", target.label()),
                        format!("Clear command history for {}?", target.label()),
                    ))
                    .child(
                        Button::new("logs-clear-confirm")
                            .compact()
                            .danger()
                            .label(crate::locale::text("确认清空", "Confirm clear"))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.confirm_clear_history(cx);
                            })),
                    )
                    .child(
                        Button::new("logs-clear-cancel")
                            .compact()
                            .label(crate::locale::text("取消", "Cancel"))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.confirm_clear = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        if pagination_visible(self.entries.len()) {
            content = content.child(
                div()
                    .id("logs-history-pagination")
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .text_color(theme.muted)
                    .child(div().flex_1().min_w(px(0.)).text_size(theme.text_xs).child(
                        crate::locale::text(
                            format!(
                                "第 {current_page}/{page_count} 页 · 共 {} 条",
                                self.entries.len()
                            ),
                            format!(
                                "Page {current_page}/{page_count} · {} entries",
                                self.entries.len()
                            ),
                        ),
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new("logs-page-previous")
                                    .compact()
                                    .label(crate::locale::text("上一页", "Previous"))
                                    .disabled(current_page <= 1)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.set_page(view.page.saturating_sub(1).max(1), cx);
                                    })),
                            )
                            .child(
                                Button::new("logs-page-next")
                                    .compact()
                                    .label(crate::locale::text("下一页", "Next"))
                                    .disabled(current_page >= page_count)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.set_page(view.page.saturating_add(1), cx);
                                    })),
                            ),
                    ),
            );
        }
        let unavailable = match self.commands.history_availability() {
            CommandHistoryAvailability::Available => None,
            CommandHistoryAvailability::Unavailable { reason } => {
                Some(super::render::history_unavailable_message(reason))
            }
        };
        if let Some(message) = unavailable {
            content = content.child(
                div()
                    .id("logs-history-unavailable")
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .bg(theme.panel)
                    .text_color(theme.error)
                    .text_size(theme.text_xs)
                    .child(message),
            );
        }
        content
    }
}

/// 来源菜单覆盖五类历史来源，并保留未筛选项作为显式默认。
///
/// @author kongweiguang
fn source_options() -> [(Option<CommandHistorySource>, SharedString); 6] {
    [
        (None, crate::locale::text("全部来源", "All sources")),
        (
            Some(CommandHistorySource::User),
            crate::locale::text("用户输入", "User input"),
        ),
        (
            Some(CommandHistorySource::Broadcast),
            crate::locale::text("批量发送", "Broadcast"),
        ),
        (
            Some(CommandHistorySource::Snippet),
            crate::locale::text("片段", "Snippet"),
        ),
        (
            Some(CommandHistorySource::Workflow),
            crate::locale::text("工作流", "Workflow"),
        ),
        (
            Some(CommandHistorySource::Tool),
            crate::locale::text("工具", "Tool"),
        ),
    ]
}

/// 优先显示已记录工作目录；缺失时标记主机或会话回退，避免把不透明标识误认为路径。
///
/// @author kongweiguang
fn history_entry_context(
    entry: &CommandHistoryEntry,
    target: Option<&LogsTarget>,
    host_display_names: &HashMap<HostId, SharedString>,
) -> String {
    if let Some(cwd) = entry.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
        return cwd.to_owned();
    }
    if let Some(host_id) = entry.remote_host_id.as_ref() {
        let name = host_display_names.get(host_id);
        return name.map_or_else(
            || {
                crate::locale::text(format!("主机 · {host_id}"), format!("Host · {host_id}"))
                    .to_string()
            },
            |name| {
                crate::locale::text(format!("主机 · {name}"), format!("Host · {name}")).to_string()
            },
        );
    }
    if let Some(session_id) = entry.session_id.as_ref() {
        return crate::locale::text(
            format!("会话 · {session_id}"),
            format!("Session · {session_id}"),
        )
        .to_string();
    }
    target.map_or_else(
        || crate::locale::text("未绑定上下文", "Unbound context").to_string(),
        |target| target.label().to_string(),
    )
}

/// 名称可见时在悬浮/辅助文本补出稳定 HostId；路径优先的卡片也不会丢失主机身份信息。
///
/// @author kongweiguang
fn history_entry_context_detail(
    entry: &CommandHistoryEntry,
    target: Option<&LogsTarget>,
    host_display_names: &HashMap<HostId, SharedString>,
) -> String {
    let context = history_entry_context(entry, target, host_display_names);
    let Some(host_id) = entry.remote_host_id.as_ref() else {
        return context;
    };
    if let Some(name) = host_display_names.get(host_id) {
        crate::locale::text(
            format!("{context} · 主机 {name} · ID {host_id}"),
            format!("{context} · Host {name} · ID {host_id}"),
        )
        .to_string()
    } else {
        context
    }
}

/// 只有存在下一页时显示分页控件，避免空列表和单页列表占用窄面板底部空间。
///
/// @author kongweiguang
fn pagination_visible(entry_count: usize) -> bool {
    entry_count > PAGE_SIZE
}

/// 历史来源的 UI 标签与领域枚举一一对应，不从数据库字符串猜测协议。
///
/// @author kongweiguang
fn source_label(source: CommandHistorySource) -> SharedString {
    match source {
        CommandHistorySource::User => crate::locale::text("用户", "User"),
        CommandHistorySource::Broadcast => crate::locale::text("批量", "Broadcast"),
        CommandHistorySource::Snippet => crate::locale::text("片段", "Snippet"),
        CommandHistorySource::Workflow => crate::locale::text("工作流", "Workflow"),
        CommandHistorySource::Tool => crate::locale::text("工具", "Tool"),
    }
}

/// 空态只说明当前 pane 筛选没有记录，不把存储不可用误报为空列表。
///
/// @author kongweiguang
fn empty_state(message: SharedString, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .justify_center()
        .py_6()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(message)
}

/// 与 HistoryScope 的查询规范保持一致：空白搜索不算活动筛选，非默认来源则始终算筛选。
///
/// @author kongweiguang
fn history_filters_active(query: &str, source: Option<CommandHistorySource>) -> bool {
    !query.trim().is_empty() || source.is_some()
}

#[cfg(test)]
#[path = "../../../tests/unit/command_logs.rs"]
mod tests;
