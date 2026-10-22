//! GPUI Kit 搜索面板与有界、可取消的应用目录查询。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Div, Entity, EventEmitter, IntoElement, Render, Stateful, Window,
    component::{
        Disableable as _,
        command::{Command, CommandItem, CommandState},
    },
    div, hsla,
    prelude::*,
    px,
};
use kerminal_application::{AppError, commands::CommandHistoryAvailability};
use kerminal_domain::{
    agents::AgentSessionStatus,
    commands::{
        CommandHistoryListRequest, SnippetCatalogListRequest, SnippetRisk, WorkflowListRequest,
    },
    connections::RemoteProtocol,
};
use tokio_util::sync::CancellationToken;

use crate::{AppServices, keybindings::KeybindingAction, theme::ThemeTokens};

use super::{
    QuickOpenEntry, QuickOpenEvent, QuickOpenMode, QuickOpenOpenContext, QuickOpenSelection,
    model::{
        MAX_QUICK_OPEN_ENTRIES, QuickOpenSourceStatus, push_matching, text, workspace_entries,
    },
};

/// 同一窗口中的 Quick Open 与命令面板共享一个可搜索组件和唯一查询代次。
pub struct QuickOpenView {
    services: AppServices,
    command_state: Entity<CommandState>,
    mode: Option<QuickOpenMode>,
    context: Option<QuickOpenOpenContext>,
    query: String,
    entries: Vec<QuickOpenEntry>,
    source_status: Vec<QuickOpenSourceStatus>,
    loading: bool,
    generation: u64,
    query_cancellation: Option<CancellationToken>,
}

impl EventEmitter<QuickOpenEvent> for QuickOpenView {}

impl QuickOpenView {
    /// Root 仅查询遮罩是否可见，避免短暂提示在被命令面板覆盖时消耗展示时间。
    ///
    /// @author kongweiguang
    pub(crate) fn is_open(&self) -> bool {
        self.mode.is_some()
    }

    /// 注入桌面已装配服务并复用 Kit CommandState，重开面板时搜索焦点和实体均稳定。
    pub fn new(services: AppServices, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            services,
            command_state: cx.new(|cx| CommandState::new(window, cx)),
            mode: None,
            context: None,
            query: String::new(),
            entries: Vec::new(),
            source_status: Vec::new(),
            loading: false,
            generation: 0,
            query_cancellation: None,
        }
    }

    /// 每次打开都使用 Root 新捕获的 workspace 与同代 session binding，旧搜索不会跨窗口状态复用。
    pub fn open(
        &mut self,
        mode: QuickOpenMode,
        context: QuickOpenOpenContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_query();
        self.mode = Some(mode);
        self.context = Some(context);
        self.query.clear();
        self.source_status.clear();
        self.command_state
            .update(cx, |state, cx| state.set_query("", window, cx));
        self.command_state
            .update(cx, |state, cx| state.focus(window, cx));
        self.start_query(String::new(), window, cx);
        cx.notify();
    }

    /// 关闭时取消 provider 代次并立即清除短期命令文本与目标快照。
    ///
    /// @author kongweiguang
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let origin_binding = self
            .context
            .as_ref()
            .and_then(|context| context.terminal_binding.clone());
        self.cancel_query();
        self.mode = None;
        self.context = None;
        self.query.clear();
        self.entries.clear();
        self.source_status.clear();
        self.loading = false;
        self.command_state
            .update(cx, |state, cx| state.set_query("", window, cx));
        cx.emit(QuickOpenEvent::Dismissed { origin_binding });
        cx.notify();
    }

    /// 确认仍在当前渲染代次中的稳定目标；动态目录重载后不使用旧 row index 重定向。
    ///
    /// @author kongweiguang
    fn confirm(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self
            .entries
            .get(index)
            .filter(|entry| !entry.disabled)
            .and_then(|entry| entry.selection.clone())
        else {
            return;
        };
        let origin = self.context.as_ref().and_then(|context| {
            context.terminal.as_ref().filter(|target| {
                target.binding.session_id == target.context.session_id
                    && target.binding.owner == target.context.owner
                    && target.binding.session_generation == target.context.session_generation
            })
        });
        let origin = origin.cloned().map(Box::new);
        let terminal_binding = self
            .context
            .as_ref()
            .and_then(|context| context.terminal_binding.clone());
        self.cancel_query();
        self.mode = None;
        self.context = None;
        self.query.clear();
        self.entries.clear();
        self.source_status.clear();
        self.loading = false;
        self.command_state
            .update(cx, |state, cx| state.set_query("", window, cx));
        cx.emit(QuickOpenEvent::Selected {
            selection,
            origin,
            terminal_binding,
        });
        cx.notify();
    }

    /// 新查询取消旧 token，并用递增代次隔离异步 service 的迟到结果。
    ///
    /// @author kongweiguang
    fn start_query(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_none() {
            return;
        }
        self.cancel_query();
        self.query = query.trim().to_owned();
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        let cancellation = CancellationToken::new();
        self.query_cancellation = Some(cancellation.clone());
        let mode = self.mode.unwrap_or(QuickOpenMode::QuickOpen);
        let context = self.context.clone().unwrap_or(QuickOpenOpenContext {
            workspace: None,
            terminal: None,
            terminal_binding: None,
        });
        let query = self.query.clone();
        let local_entries = self.local_entries(mode, &context, &query);
        self.entries = local_entries.clone();
        self.source_status.clear();
        self.loading = true;
        self.command_state
            .update(cx, |state, cx| state.set_loading(true, window, cx));

        let services = self.services.clone();
        let executor = services.executor.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |worker_cancel| async move {
                    let mut entries = local_entries;
                    let mut statuses = vec![QuickOpenSourceStatus::Info(text(
                        "路径搜索覆盖已打开文件和终端当前目录。",
                        "Path search covers open files and current terminal directories.",
                    ))];

                    if worker_cancel.is_cancelled() {
                        return Err(AppError::cancelled("Quick Open query was cancelled"));
                    }
                    append_hosts(
                        &services,
                        &query,
                        &worker_cancel,
                        &mut entries,
                        &mut statuses,
                    )
                    .await;
                    if worker_cancel.is_cancelled() {
                        return Err(AppError::cancelled("Quick Open query was cancelled"));
                    }
                    append_command_catalogs(
                        &services,
                        &context,
                        &query,
                        &worker_cancel,
                        &mut entries,
                        &mut statuses,
                    )
                    .await;
                    if worker_cancel.is_cancelled() {
                        return Err(AppError::cancelled("Quick Open query was cancelled"));
                    }
                    append_agents(
                        &services,
                        &query,
                        &worker_cancel,
                        &mut entries,
                        &mut statuses,
                    )
                    .await;
                    entries.truncate(MAX_QUICK_OPEN_ENTRIES);
                    Ok((entries, statuses))
                })
                .await;
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |view, cx| {
                    if view.generation != generation || view.mode.is_none() {
                        return;
                    }
                    view.loading = false;
                    view.query_cancellation = None;
                    match result {
                        Ok((entries, statuses)) => {
                            view.entries = entries;
                            view.source_status = statuses;
                        }
                        Err(error)
                            if error.public_code()
                                == kerminal_application::PublicErrorCode::Cancelled => {}
                        Err(error) => {
                            view.source_status = vec![QuickOpenSourceStatus::Error(
                                error.public_message().to_owned(),
                            )]
                        }
                    }
                    view.command_state
                        .update(cx, |state, cx| state.set_loading(false, window, cx));
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 将取消信号下发给当前 service 查询；不会以丢弃 future 的方式伪装业务取消。
    fn cancel_query(&mut self) {
        if let Some(cancellation) = self.query_cancellation.take() {
            cancellation.cancel();
        }
        self.generation = self.generation.wrapping_add(1).max(1);
    }

    /// 当前快照和固定动作目录构造即时行；随后异步查询再补真实服务目录。
    fn local_entries(
        &self,
        mode: QuickOpenMode,
        context: &QuickOpenOpenContext,
        query: &str,
    ) -> Vec<QuickOpenEntry> {
        local_search_entries(context, mode, query)
    }
}

impl Render for QuickOpenView {
    /// Kit 的取消回调在 CommandState 更新期间同步触发；延后完整关闭，避免重入清理查询状态。
    /// 全窗遮罩建立阻挡命中区域，避免输入框点击与窗口非客户区命中穿到底层 Canvas。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.mode.is_none() {
            return div().id("quick-open-closed").size_0();
        }
        let theme = ThemeTokens::current(window, cx);
        let view = cx.entity().clone();
        let query_view = view.clone();
        let confirm_view = view.clone();
        let cancel_view = view.clone();
        let command_items = self.entries.iter().map(|entry| {
            let mut item = CommandItem::new()
                .label(format!("{}  ·  {}", entry.label, entry.detail))
                .keywords(entry.keywords.clone());
            if entry.disabled || entry.selection.is_none() {
                item = item.disabled(true);
            }
            item
        });
        let placeholder = match self.mode {
            Some(QuickOpenMode::QuickOpen) => text(
                "搜索标签、分屏、文件、主机、命令或 Agent…",
                "Search tabs, panes, files, hosts, commands, or Agents…",
            ),
            Some(QuickOpenMode::CommandPalette) => text(
                "搜索动作、标签、文件、主机或命令…",
                "Search actions, tabs, files, hosts, or commands…",
            ),
            None => String::new(),
        };
        let footer_status = self.source_status.clone();
        let command = Command::new(&self.command_state)
            .items(command_items)
            .filterable(false)
            .placeholder(placeholder)
            .max_h(px(480.))
            .footer(move |_, _, _| {
                div()
                    .id("quick-open-source-status")
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(footer_status.iter().map(|status| {
                        let (message, color) = match status {
                            QuickOpenSourceStatus::Info(message) => (message.clone(), theme.muted),
                            QuickOpenSourceStatus::Error(message) => (message.clone(), theme.error),
                        };
                        div().text_color(color).child(message)
                    }))
            })
            .on_query(move |query, window, app| {
                let query = query.to_string();
                query_view.update(app, |view, cx| view.start_query(query, window, cx));
            })
            .on_confirm(move |index, window, app| {
                confirm_view.update(app, |view, cx| view.confirm(index.row, window, cx));
            })
            .on_cancel(move |window, app| {
                let cancel_view = cancel_view.clone();
                window.defer(app, move |window, app| {
                    cancel_view.update(app, |view, cx| view.close(window, cx));
                });
            });

        quick_open_overlay_surface().child(
            div()
                .id("quick-open-modal")
                .key_context("Modal")
                .mt(px(54.))
                .w(px(720.))
                .max_w_full()
                .max_h(px(560.))
                .bg(theme.panel)
                .border_1()
                .border_color(theme.muted)
                .rounded_md()
                .shadow_lg()
                .text_color(theme.foreground)
                .child(command),
        )
    }
}

/// 遮罩必须阻挡鼠标和滚动命中；单纯绘制半透明背景不会在 GPUI 中遮蔽后方 Canvas 或标题栏。
/// 保持面板子控件正常接收输入，不用全窗捕获事件吞掉 Input 自己的聚焦、选中或组合输入。
///
/// @author kongweiguang
pub(super) fn quick_open_overlay_surface() -> Stateful<Div> {
    div()
        .id("quick-open-overlay")
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .justify_center()
        .items_start()
        .bg(hsla(0., 0., 0., 0.42))
}

/// 仅展示当前被查询的数据源；Root 通过精确 ID 再次校验目标存在和状态。
///
/// @author kongweiguang
pub(super) fn local_search_entries(
    context: &QuickOpenOpenContext,
    mode: QuickOpenMode,
    query: &str,
) -> Vec<QuickOpenEntry> {
    let mut entries = Vec::new();
    if mode == QuickOpenMode::CommandPalette {
        for action in KeybindingAction::ALL {
            if matches!(
                action,
                KeybindingAction::WorkspaceQuickOpen | KeybindingAction::WorkspaceCommandPalette
            ) {
                continue;
            }
            let label = action_label(action);
            let terminal_action_without_target = matches!(
                action,
                KeybindingAction::TerminalSearch | KeybindingAction::TerminalPaste
            ) && context.terminal_binding.is_none();
            let detail = if terminal_action_without_target {
                text("需要打开中的终端", "Requires an open terminal")
            } else {
                action_category(action)
            };
            push_matching(
                &mut entries,
                QuickOpenEntry {
                    label: label.clone(),
                    detail,
                    keywords: vec![action.id().to_owned()],
                    selection: Some(QuickOpenSelection::Action(action)),
                    disabled: terminal_action_without_target,
                },
                query,
            );
        }
    }
    if let Some(workspace) = context.workspace.as_ref() {
        for entry in workspace_entries(workspace, query) {
            push_matching(&mut entries, entry, query);
        }
    }
    entries.truncate(MAX_QUICK_OPEN_ENTRIES);
    entries
}

/// 命令名称对应 Root 的实际作用范围，标签导航不再限定终端，稳定 action ID 仍用于配置和搜索。
///
/// @author kongweiguang
fn action_label(action: KeybindingAction) -> String {
    let (zh, en) = match action {
        KeybindingAction::ApplicationQuit => ("退出 Kerminal", "Quit Kerminal"),
        KeybindingAction::SettingsOpen => ("打开设置", "Open settings"),
        KeybindingAction::SettingsKeybindings => ("快捷键设置", "Keyboard shortcuts"),
        KeybindingAction::TerminalFocus => ("聚焦终端", "Focus terminal"),
        KeybindingAction::TerminalSearch => ("搜索终端", "Search terminal"),
        KeybindingAction::TerminalPaste => ("粘贴到终端", "Paste into terminal"),
        KeybindingAction::TerminalNewTab => ("新建终端标签", "New terminal tab"),
        KeybindingAction::TerminalCloseTab => ("关闭终端标签", "Close terminal tab"),
        KeybindingAction::TerminalClosePane => ("关闭终端分屏", "Close terminal pane"),
        KeybindingAction::TerminalSplitHorizontal => ("左右分屏", "Split horizontally"),
        KeybindingAction::TerminalSplitVertical => ("上下分屏", "Split vertically"),
        KeybindingAction::TerminalPreviousTab => ("上一个标签", "Previous tab"),
        KeybindingAction::TerminalNextTab => ("下一个标签", "Next tab"),
        KeybindingAction::ToolAgentLauncher => ("打开 Agent 启动器", "Open Agent launcher"),
        KeybindingAction::ToolSystem => ("打开系统信息", "Open system information"),
        KeybindingAction::ToolSftp => ("打开 SFTP", "Open SFTP"),
        KeybindingAction::ToolPorts => ("打开端口转发", "Open port forwards"),
        KeybindingAction::ToolSnippets => ("打开命令片段", "Open command snippets"),
        KeybindingAction::ToolLogs => ("打开终端日志", "Open terminal logs"),
        KeybindingAction::WorkspaceQuickOpen => ("快速打开", "Quick Open"),
        KeybindingAction::WorkspaceCommandPalette => ("命令面板", "Command Palette"),
    };
    text(zh, en)
}

/// 类别按实际作用范围归属；跨类型标签导航归工作区，搜索仍保留旧 action ID。
///
/// @author kongweiguang
fn action_category(action: KeybindingAction) -> String {
    let (zh, en) = match action {
        KeybindingAction::ApplicationQuit => ("应用", "Application"),
        KeybindingAction::SettingsOpen | KeybindingAction::SettingsKeybindings => {
            ("设置", "Settings")
        }
        KeybindingAction::TerminalFocus
        | KeybindingAction::TerminalSearch
        | KeybindingAction::TerminalPaste
        | KeybindingAction::TerminalNewTab
        | KeybindingAction::TerminalCloseTab
        | KeybindingAction::TerminalClosePane
        | KeybindingAction::TerminalSplitHorizontal
        | KeybindingAction::TerminalSplitVertical => ("终端", "Terminal"),
        KeybindingAction::ToolAgentLauncher
        | KeybindingAction::ToolSystem
        | KeybindingAction::ToolSftp
        | KeybindingAction::ToolPorts
        | KeybindingAction::ToolSnippets
        | KeybindingAction::ToolLogs => ("工具", "Tools"),
        KeybindingAction::WorkspaceQuickOpen
        | KeybindingAction::WorkspaceCommandPalette
        | KeybindingAction::TerminalPreviousTab
        | KeybindingAction::TerminalNextTab => ("工作区", "Workspace"),
    };
    text(zh, en)
}

/// 协议名称保持用户可读；主机凭据、vault 引用和连接路由不会进入搜索行。
///
/// @author kongweiguang
fn remote_protocol_label(protocol: RemoteProtocol) -> String {
    let (zh, en) = match protocol {
        RemoteProtocol::Ssh => ("SSH 主机", "SSH host"),
        RemoteProtocol::Sftp => ("SFTP 文件服务", "SFTP file service"),
        RemoteProtocol::Telnet => ("Telnet 主机", "Telnet host"),
        RemoteProtocol::Serial => ("串口设备", "Serial device"),
        RemoteProtocol::Rdp => ("RDP 远程桌面", "RDP desktop"),
    };
    text(zh, en)
}

/// 风险文案对用户解释类别，不暴露序列化枚举名或内部值。
///
/// @author kongweiguang
fn snippet_risk_label(risk: SnippetRisk) -> String {
    let (zh, en) = match risk {
        SnippetRisk::Inspect => ("只读检查", "Read-only inspection"),
        SnippetRisk::Change => ("会修改内容", "Changes content"),
        SnippetRisk::Destructive => ("高风险操作", "High-risk operation"),
        SnippetRisk::Unknown => ("风险未说明", "Risk not specified"),
    };
    text(zh, en)
}

/// 主机目录只投影名称、可读协议与标签；认证、vault 引用和 connection route 不进入搜索 UI。
///
/// @author kongweiguang
async fn append_hosts(
    services: &AppServices,
    query: &str,
    cancellation: &CancellationToken,
    entries: &mut Vec<QuickOpenEntry>,
    statuses: &mut Vec<QuickOpenSourceStatus>,
) {
    let Some(hosts) = services.hosts.as_ref() else {
        statuses.push(QuickOpenSourceStatus::Error(text(
            "主机目录未配置",
            "Host catalog is not configured",
        )));
        return;
    };
    match hosts.list_hosts().await {
        Ok(hosts) => {
            for host in hosts {
                if cancellation.is_cancelled() || entries.len() >= MAX_QUICK_OPEN_ENTRIES {
                    break;
                }
                push_matching(
                    entries,
                    QuickOpenEntry {
                        label: host.name,
                        detail: remote_protocol_label(host.protocol),
                        keywords: host.tags,
                        selection: Some(QuickOpenSelection::Host(host.id)),
                        disabled: false,
                    },
                    query,
                );
            }
        }
        Err(error) => statuses.push(source_error("主机目录", "Host catalog", &error)),
    }
}

/// 历史、片段和工作流保留各自服务状态，并把片段风险转换成用户文案而非内部 enum 名。
///
/// @author kongweiguang
async fn append_command_catalogs(
    services: &AppServices,
    context: &QuickOpenOpenContext,
    query: &str,
    cancellation: &CancellationToken,
    entries: &mut Vec<QuickOpenEntry>,
    statuses: &mut Vec<QuickOpenSourceStatus>,
) {
    let Some(commands) = services.commands.as_ref() else {
        statuses.push(QuickOpenSourceStatus::Error(text(
            "命令目录未配置",
            "Command catalog is not configured",
        )));
        return;
    };
    let insertion_target = context.terminal.as_ref().filter(|target| {
        target.context.prompt_line_empty
            && target.context.availability
                == kerminal_application::sessions::TerminalCommandAvailability::Available
            && target.binding.session_id == target.context.session_id
            && target.binding.owner == target.context.owner
            && target.binding.session_generation == target.context.session_generation
    });
    match commands.history_availability() {
        CommandHistoryAvailability::Available => {
            let request = CommandHistoryListRequest {
                query: (!query.is_empty()).then(|| query.to_owned()),
                limit: Some(80),
                ..Default::default()
            };
            match commands.list_history(request).await {
                Ok(history) => {
                    for entry in history {
                        if cancellation.is_cancelled() || entries.len() >= MAX_QUICK_OPEN_ENTRIES {
                            break;
                        }
                        let command_safe =
                            QuickOpenEntry::history_command_is_insertable(&entry.command);
                        let can_insert = insertion_target.is_some() && command_safe;
                        let detail = if !command_safe {
                            text(
                                "多行或含控制字符，不能安全插入",
                                "Multiline or control text cannot be inserted safely",
                            )
                        } else if insertion_target.is_none() {
                            text(
                                "需要空白且可信的终端提示符",
                                "Requires a clean, trusted terminal prompt",
                            )
                        } else {
                            format!(
                                "{} · {}",
                                text("历史命令", "Command history"),
                                entry.cwd.as_deref().unwrap_or("")
                            )
                        };
                        push_matching(
                            entries,
                            QuickOpenEntry {
                                label: entry.command.clone(),
                                detail,
                                keywords: vec![entry.id, entry.shell.unwrap_or_default()],
                                selection: Some(QuickOpenSelection::HistoryCommand(entry.command)),
                                disabled: !can_insert,
                            },
                            query,
                        );
                    }
                }
                Err(error) => statuses.push(source_error("命令历史", "Command history", &error)),
            }
        }
        CommandHistoryAvailability::Unavailable { reason } => {
            let english = match reason {
                kerminal_application::commands::CommandHistoryUnavailableReason::MigrationRequired(_) => {
                    "Command history requires the 0.4 data migration before it can be used"
                }
                _ => "Command history is unavailable",
            };
            statuses.push(QuickOpenSourceStatus::Error(text(
                match reason {
                    kerminal_application::commands::CommandHistoryUnavailableReason::MigrationRequired(_) => "命令历史需要先执行0.4数据迁移",
                    kerminal_application::commands::CommandHistoryUnavailableReason::UnsupportedSchemaVersion(_) => "命令历史数据库版本不受支持",
                    kerminal_application::commands::CommandHistoryUnavailableReason::CorruptDatabase => "命令历史数据库已损坏",
                    kerminal_application::commands::CommandHistoryUnavailableReason::StorageUnavailable => "命令历史存储不可用",
                    kerminal_application::commands::CommandHistoryUnavailableReason::BackupFailed => "命令历史安全备份失败",
                    kerminal_application::commands::CommandHistoryUnavailableReason::StorageLockUnavailable => "命令历史存储锁不可用",
                },
                english,
            )));
        }
    }
    if cancellation.is_cancelled() {
        return;
    }

    match commands
        .list_snippets(SnippetCatalogListRequest {
            query: (!query.is_empty()).then(|| query.to_owned()),
            limit: Some(100),
            ..Default::default()
        })
        .await
    {
        Ok(snippets) => {
            for snippet in snippets {
                if cancellation.is_cancelled() || entries.len() >= MAX_QUICK_OPEN_ENTRIES {
                    break;
                }
                push_matching(
                    entries,
                    QuickOpenEntry {
                        label: snippet.title,
                        detail: format!(
                            "{} · {}",
                            text("命令片段", "Snippet"),
                            snippet_risk_label(snippet.risk)
                        ),
                        keywords: snippet.tags,
                        selection: Some(QuickOpenSelection::Snippet(snippet.id)),
                        disabled: false,
                    },
                    query,
                );
            }
        }
        Err(error) => statuses.push(source_error("命令片段", "Snippets", &error)),
    }
    if cancellation.is_cancelled() {
        return;
    }

    match commands
        .list_workflows(WorkflowListRequest {
            query: (!query.is_empty()).then(|| query.to_owned()),
            ..Default::default()
        })
        .await
    {
        Ok(workflows) => {
            for workflow in workflows.into_iter().take(100) {
                if cancellation.is_cancelled() || entries.len() >= MAX_QUICK_OPEN_ENTRIES {
                    break;
                }
                push_matching(
                    entries,
                    QuickOpenEntry {
                        label: workflow.title,
                        detail: format!(
                            "{} · {} {}",
                            text("工作流", "Workflow"),
                            workflow.steps.len(),
                            text("步骤", "steps")
                        ),
                        keywords: workflow.tags,
                        selection: Some(QuickOpenSelection::Workflow(workflow.id)),
                        disabled: false,
                    },
                    query,
                );
            }
        }
        Err(error) => statuses.push(source_error("工作流目录", "Workflow catalog", &error)),
    }
}

/// Agent 目录调用传递同一取消 token，只允许从真实服务列表导航已有会话。
async fn append_agents(
    services: &AppServices,
    query: &str,
    cancellation: &CancellationToken,
    entries: &mut Vec<QuickOpenEntry>,
    statuses: &mut Vec<QuickOpenSourceStatus>,
) {
    let Some(agents) = services.agents.as_ref() else {
        statuses.push(QuickOpenSourceStatus::Error(text(
            "Agent 服务未配置",
            "Agent service is not configured",
        )));
        return;
    };
    match agents.list(cancellation.clone()).await {
        Ok(sessions) => {
            for item in sessions.into_iter().take(100) {
                if cancellation.is_cancelled() || entries.len() >= MAX_QUICK_OPEN_ENTRIES {
                    break;
                }
                let archived = item.session.status == AgentSessionStatus::Archived;
                push_matching(
                    entries,
                    QuickOpenEntry {
                        label: item.session.title,
                        detail: if archived {
                            text("Agent 会话 · 已归档", "Agent session · archived")
                        } else {
                            text("Agent 会话", "Agent session")
                        },
                        keywords: vec![format!("{:?}", item.session.agent_id)],
                        selection: Some(QuickOpenSelection::Agent(item.session.agent_session_id)),
                        disabled: archived,
                    },
                    query,
                );
            }
        }
        Err(error) => statuses.push(source_error("Agent 会话", "Agent sessions", &error)),
    }
}

/// 面向 Quick Open 的服务失败仅展示受控错误消息，不泄漏错误对象或连接凭据。
fn source_error(
    chinese: &'static str,
    english: &'static str,
    error: &AppError,
) -> QuickOpenSourceStatus {
    QuickOpenSourceStatus::Error(format!(
        "{}: {}",
        text(chinese, english),
        error.public_message()
    ))
}

#[cfg(test)]
#[path = "../../../tests/unit/quick_open_view.rs"]
mod tests;
