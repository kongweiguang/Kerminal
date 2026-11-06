//! 终端工具栏中的独立命令历史视图，不复用命令工作区的 Entity 状态。
//!
//! @author kongweiguang

use std::{collections::HashMap, sync::Arc};

use gpui_kit::{
    AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    IntoElement, Render, SharedString, Subscription, Window,
    assets::IconName,
    canvas,
    component::input::{Input, InputEvent, InputState},
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
    AppError, ApplicationTaskExecutor, PublicErrorCode,
    commands::{
        CommandHistoryAvailability, CommandsService, SnippetExecutionService, SnippetRenderResult,
    },
    preferences::ThemeMode,
    sessions::UiSessionBinding,
};
use kerminal_domain::{
    commands::{CommandHistoryEntry, CommandHistorySource, CommandHistoryTarget},
    identity::{HostId, PaneId},
    sessions::{SessionInfo, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;

use super::history_query::{HistoryScope, PAGE_SIZE, page_window as logs_page_window};
use crate::theme::{ThemeTokens, UiMetrics};

#[cfg(feature = "right-rail-acceptance")]
#[path = "logs/right_rail_acceptance.rs"]
pub(super) mod right_rail_acceptance;

const HISTORY_LIMIT: usize = 100;

/// 列表绘制与分页测量共用同一份密度投影，避免文字放大后沿用旧的固定行容量。
#[derive(Clone, Copy)]
struct HistoryGeometry {
    row_height: gpui_kit::Pixels,
    header_height: gpui_kit::Pixels,
    pagination_height: gpui_kit::Pixels,
}

impl HistoryGeometry {
    /// 控件、留白与物理边线分别取自共享度量和单像素边界，紧凑默认保持原有可见容量。
    ///
    /// @author kongweiguang
    fn from_metrics(metrics: UiMetrics) -> Self {
        Self {
            row_height: metrics.control_height + metrics.panel_padding + metrics.gap,
            header_height: metrics.control_height + metrics.gap / 2. + px(1.),
            pagination_height: metrics.control_height + metrics.panel_padding + px(1.),
        }
    }
}

/// Logs 当前绑定的 pane、目标和可信 host；session generation 只用于迟到响应保护。
#[derive(Clone, Debug)]
pub struct LogsTarget {
    binding: UiSessionBinding,
    scope: HistoryScope,
    target: CommandHistoryTarget,
    detail: String,
}

impl PartialEq for LogsTarget {
    /// 展示标题不属于历史请求身份，pane 改名不能使在途查询或清理回执失效。
    ///
    /// @author kongweiguang
    fn eq(&self, other: &Self) -> bool {
        self.binding == other.binding && self.scope == other.scope && self.target == other.target
    }
}

impl Eq for LogsTarget {}

impl LogsTarget {
    /// Root 复用已验证的历史目标绑定，为片段预填冻结同一 pane/session 身份。
    ///
    /// @author kongweiguang
    pub(crate) fn binding(&self) -> &UiSessionBinding {
        &self.binding
    }

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

    /// 把会话目标映射为冻结 0.3.45 历史筛选，不把一次 session ID 写进 pane 级查询；detail 仅用于目标展示。
    ///
    /// @author kongweiguang
    fn from_session_parts(
        binding: UiSessionBinding,
        session_target: &SessionTarget,
        pane_id: PaneId,
        remote_host_id: Option<HostId>,
    ) -> Self {
        let detail = match session_target {
            SessionTarget::Local(target) => target.program.to_string_lossy().into_owned(),
            SessionTarget::Ssh(target) => target.host_id.to_string(),
            SessionTarget::Telnet(target) => remote_host_id.as_ref().map_or_else(
                || format!("{}:{}", target.host, target.port),
                ToString::to_string,
            ),
            SessionTarget::Serial(target) => remote_host_id
                .as_ref()
                .map_or_else(|| target.device.display().to_string(), ToString::to_string),
            SessionTarget::Container(target) => target.container_id.clone(),
        };
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
            detail,
        }
    }

    /// 返回当前 pane 级请求范围，清理确认和搜索始终从同一 target 快照创建。
    ///
    /// @author kongweiguang
    fn scope(&self) -> &HistoryScope {
        &self.scope
    }

    /// 片段管理器按 main 只限定 pane，避免额外 target/host 筛选漏掉该分屏的历史。
    ///
    /// @author kongweiguang
    pub(crate) fn snippet_history_request(
        &self,
        limit: usize,
    ) -> kerminal_domain::commands::CommandHistoryListRequest {
        let scoped = self.scope.list_request(String::new(), None, limit);
        kerminal_domain::commands::CommandHistoryListRequest {
            pane_id: scoped.pane_id,
            limit: Some(limit),
            ..Default::default()
        }
    }

    /// 构造与 main 一致的简短目标类型标签；具体身份由单独的 detail 展示。
    ///
    /// @author kongweiguang
    fn label(&self) -> SharedString {
        match self.target {
            CommandHistoryTarget::Local => crate::locale::text("本地", "Local"),
            CommandHistoryTarget::Ssh => crate::locale::text("SSH", "SSH"),
            CommandHistoryTarget::Telnet => crate::locale::text("Telnet", "Telnet"),
            CommandHistoryTarget::Serial => crate::locale::text("串口", "Serial"),
            CommandHistoryTarget::Container => crate::locale::text("容器", "Container"),
        }
    }

    /// 提供稳定的会话目标身份，不从筛选文字或当前选择推断 HostId。
    ///
    /// @author kongweiguang
    fn detail(&self) -> &str {
        &self.detail
    }

    /// 只接受同一 UI pane 的展示标题，避免焦点切换期间错配到冻结的历史目标。
    ///
    /// @author kongweiguang
    pub(crate) fn with_pane_detail(mut self, pane_id: &PaneId, detail: String) -> Self {
        let matches_pane = matches!(
            &self.binding.owner,
            SessionOwner::Ui {
                pane_id: target_pane,
                ..
            } if target_pane == pane_id
        );
        if matches_pane && !detail.trim().is_empty() {
            self.detail = detail;
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoryReadKey {
    target: LogsTarget,
    query: String,
    source: Option<CommandHistorySource>,
}

/// 终端写入结果携带点击时的 binding 和焦点，Root 仍需确认当前视图及焦点后再归还终端。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub(crate) struct LogsTerminalFillCompleted {
    pub(crate) binding: UiSessionBinding,
    pub(crate) expected_focus: Option<FocusHandle>,
    pub(crate) generation: u64,
}

/// 单次填入独占写入槽，切换目标只请求取消并等待原 writer 的真实回执。
struct HistoryFillOperation {
    generation: u64,
    entry_id: String,
    target: LogsTarget,
    cancellation: CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HistoryFillStatus {
    Confirmed(String),
    Problem(String),
}

/// 将焦点票据和反馈状态放在一起，使列表交互只能丢弃已确认的提示。
#[derive(Default)]
struct HistoryFillFeedback {
    generation: u64,
    status: Option<HistoryFillStatus>,
    unknown_target: Option<LogsTarget>,
}

impl HistoryFillFeedback {
    /// 用户开始新的列表交互后，旧成功与焦点票据失效；未解决结果仍须等待用户处理。
    ///
    /// @author kongweiguang
    fn begin_interaction(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        if matches!(self.status, Some(HistoryFillStatus::Confirmed(_))) {
            self.status = None;
        }
        self.generation
    }

    /// 只展示仍属于当前交互的成功；过期回执不能恢复提示或触发焦点转移。
    ///
    /// @author kongweiguang
    fn confirm(&mut self, generation: u64, message: String) -> bool {
        if generation != self.generation {
            return false;
        }
        self.status = Some(HistoryFillStatus::Confirmed(message));
        true
    }

    /// 失败即使迟到也要告知用户，不能因为列表变化而丢弃。
    ///
    /// @author kongweiguang
    fn fail(&mut self, message: String) {
        self.status = Some(HistoryFillStatus::Problem(message));
    }

    /// 未知结果独立保留原目标和重试门禁，避免列表交互造成重复写入。
    ///
    /// @author kongweiguang
    fn outcome_unknown(&mut self, target: LogsTarget) {
        self.unknown_target = Some(target);
        self.status = None;
    }
}

/// 独立 Logs Entity；只持有筛选与展示状态，所有历史数据仍由共享 CommandsService 读取。
pub struct LogsView {
    commands: Arc<CommandsService>,
    executor: ApplicationTaskExecutor,
    terminal_fill_service: Option<Arc<SnippetExecutionService>>,
    query: Entity<InputState>,
    query_value: String,
    target: Option<LogsTarget>,
    /// 仅用于历史卡片的友好标签；请求和清理仍以 entry 中的 HostId 为准。
    host_display_names: HashMap<HostId, SharedString>,
    theme_mode: ThemeMode,
    active: bool,
    source: Option<CommandHistorySource>,
    page: usize,
    page_size: usize,
    entries: Vec<CommandHistoryEntry>,
    loading: bool,
    error: Option<String>,
    status: Option<String>,
    /// 写入反馈与历史查询分开保存，避免刷新结果覆盖待用户检查的操作结果。
    fill_feedback: HistoryFillFeedback,
    fill_operation: Option<HistoryFillOperation>,
    clear_target_in_flight: Option<LogsTarget>,
    generation: u64,
    read_key: Option<HistoryReadKey>,
    /// 接受性 readiness 要区分“成功返回空列表”和“尚未返回”；不进入普通运行构建。
    #[cfg(feature = "right-rail-acceptance")]
    loaded_read_key: Option<HistoryReadKey>,
    #[cfg(feature = "right-rail-acceptance")]
    acceptance: right_rail_acceptance::LogsAcceptanceState,
    read_cancellation: Option<CancellationToken>,
    _query_subscription: Subscription,
}

impl EventEmitter<LogsTerminalFillCompleted> for LogsView {}

impl LogsView {
    /// 在 Root 单独创建工具 Entity 并订阅输入变化；搜索变更使旧成功反馈和焦点票据失效。
    ///
    /// @author kongweiguang
    pub fn new(
        commands: Arc<CommandsService>,
        executor: ApplicationTaskExecutor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("搜索命令", "Search commands"))
        });
        let query_subscription =
            cx.subscribe_in(&query, window, |view, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change)
                    && view.query_value != view.query.read(cx).value()
                {
                    let query = view.query.read(cx).value().to_string();
                    view.set_query(query, cx);
                }
            });
        Self {
            commands,
            executor,
            terminal_fill_service: None,
            query,
            query_value: String::new(),
            target: None,
            host_display_names: HashMap::new(),
            theme_mode: ThemeMode::System,
            active: false,
            source: None,
            page: 1,
            page_size: PAGE_SIZE,
            entries: Vec::new(),
            loading: false,
            error: None,
            status: None,
            fill_feedback: HistoryFillFeedback::default(),
            fill_operation: None,
            clear_target_in_flight: None,
            generation: 0,
            read_key: None,
            #[cfg(feature = "right-rail-acceptance")]
            loaded_read_key: None,
            #[cfg(feature = "right-rail-acceptance")]
            acceptance: right_rail_acceptance::LogsAcceptanceState::default(),
            read_cancellation: None,
            _query_subscription: query_subscription,
        }
    }

    /// 注入组合根持有的终端填入用例；日志视图不自行创建会话服务或绕过 writer。
    ///
    /// @author kongweiguang
    pub(crate) fn set_terminal_fill_service(
        &mut self,
        service: Option<Arc<SnippetExecutionService>>,
        cx: &mut Context<Self>,
    ) {
        self.terminal_fill_service = service;
        cx.notify();
    }

    /// 焦点票据必须仍属于相同活动视图及目标，避免异步完成后抢占新导航。
    ///
    /// @author kongweiguang
    pub(crate) fn terminal_fill_completion_is_current(
        &self,
        event: &LogsTerminalFillCompleted,
    ) -> bool {
        history_fill_completion_matches(
            self.active,
            self.fill_feedback.generation,
            self.target.as_ref(),
            self.fill_operation.is_some(),
            self.fill_feedback.unknown_target.is_some(),
            event,
        )
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

    /// 语言切换只刷新搜索占位，不重建输入实体或清空当前查询与焦点。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("搜索命令", "Search commands"),
                window,
                cx,
            )
        });
        cx.notify();
    }

    /// pane/session generation 改变时取消旧查询与待接纳的填入，并保留仍在收敛或待核对的写入结果。
    ///
    /// @author kongweiguang
    pub fn set_target(&mut self, target: Option<LogsTarget>, cx: &mut Context<Self>) {
        if self.target == target {
            let detail_changed = self
                .target
                .as_ref()
                .zip(target.as_ref())
                .is_some_and(|(current, next)| current.detail != next.detail);
            if detail_changed {
                self.target = target;
                cx.notify();
            }
            return;
        }
        self.cancel_read();
        self.cancel_terminal_fill();
        self.fill_feedback.begin_interaction();
        #[cfg(feature = "right-rail-acceptance")]
        self.acceptance
            .invalidate_pending("Logs target changed during acceptance seeding");
        self.generation = self.generation.wrapping_add(1).max(1);
        self.target = target;
        self.entries.clear();
        #[cfg(feature = "right-rail-acceptance")]
        {
            self.loaded_read_key = None;
        }
        self.page = 1;
        self.loading = false;
        self.error = None;
        self.status = None;
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

    /// 隐藏时允许读取收敛，并协作取消尚未接纳的填入；返回后不能沿用旧成功提示或焦点票据。
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
            self.cancel_terminal_fill();
            self.fill_feedback.begin_interaction();
            #[cfg(feature = "right-rail-acceptance")]
            if self
                .acceptance
                .invalidate_pending("Logs panel was hidden during acceptance seeding")
            {
                self.loading = false;
            }
        }
        cx.notify();
    }

    /// 根据当前已确认的记录与 pane 范围取正文，避免旧行点击复制另一个目标的数据；有效操作使旧成功提示失效。
    ///
    /// @author kongweiguang
    fn copy_entry(&mut self, entry_id: &str, cx: &mut Context<Self>) {
        let Some(target) = self.target.as_ref().filter(|_| self.active) else {
            return;
        };
        let Some(command) = self
            .entries
            .iter()
            .find(|entry| entry.id == entry_id && target.scope().contains_entry(entry))
            .map(|entry| entry.command.clone())
        else {
            return;
        };
        self.fill_feedback.begin_interaction();
        cx.write_to_clipboard(ClipboardItem::new_string(command));
        self.status = Some(crate::locale::text("命令已复制", "Command copied").to_string());
        cx.notify();
    }

    /// 仅向点击时冻结的可见终端写入单行 Paste；不追加回车，并保留迟到的失败或未知回执。
    ///
    /// @author kongweiguang
    fn fill_entry(&mut self, entry_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active
            || self.loading
            || self.clear_target_in_flight.is_some()
            || self.fill_operation.is_some()
            || self.fill_feedback.unknown_target.is_some()
        {
            return;
        }
        let (Some(target), Some(service)) =
            (self.target.clone(), self.terminal_fill_service.clone())
        else {
            return;
        };
        let Some(command) = self
            .entries
            .iter()
            .find(|entry| entry.id == entry_id && target.scope().contains_entry(entry))
            .map(|entry| entry.command.clone())
            .filter(|command| history_fill_command_is_valid(command))
        else {
            return;
        };

        let generation = self.fill_feedback.begin_interaction();
        let binding = target.binding().clone();
        let expected_focus = window.focused(cx);
        let cancellation = CancellationToken::new();
        self.fill_operation = Some(HistoryFillOperation {
            generation,
            entry_id: entry_id.to_owned(),
            target: target.clone(),
            cancellation: cancellation.clone(),
        });
        let executor = self.executor.clone();
        cx.spawn_in(window, async move |this, cx| {
            let fill_binding = binding.clone();
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    service
                        .fill_direct(
                            SnippetRenderResult {
                                command,
                                sensitive: false,
                            },
                            fill_binding,
                            operation_cancellation,
                        )
                        .await
                        .map(|_| ())
                })
                .await;
            let _ = cx.update(|_, app| {
                let _ = this.update(app, |view, cx| {
                    if !view.fill_operation.as_ref().is_some_and(|operation| {
                        operation.generation == generation && operation.target == target
                    }) {
                        return;
                    }
                    view.fill_operation = None;
                    let same_target = view.target.as_ref() == Some(&target);
                    match result {
                        Ok(()) => {
                            let current = view.fill_feedback.confirm(
                                generation,
                                crate::locale::text(
                                    "已向原终端填入命令，可在终端编辑",
                                    "Command filled in the original terminal; you can keep editing",
                                )
                                .to_string(),
                            );
                            if same_target && view.active && current {
                                cx.emit(LogsTerminalFillCompleted {
                                    binding,
                                    expected_focus,
                                    generation,
                                });
                            }
                        }
                        Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                            view.fill_feedback.outcome_unknown(target);
                        }
                        Err(error) if error.public_code() == PublicErrorCode::Cancelled => {
                            view.fill_feedback.fail(
                                crate::locale::text("终端填入已取消", "Terminal fill cancelled")
                                    .to_string(),
                            );
                        }
                        Err(error) => {
                            view.fill_feedback.fail(error.public_message().to_owned());
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 切换目标只通知用例停止尚未提交的写入；在途操作保留到收到明确或未知回执。
    ///
    /// @author kongweiguang
    fn cancel_terminal_fill(&mut self) {
        if let Some(operation) = self.fill_operation.as_ref() {
            operation.cancellation.cancel();
        }
    }

    /// 仅接受当前活动 UI binding 的成功查询；空历史是有效的实际回执，未完成或失败则不通过。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn right_rail_acceptance_history_ready(&self, binding: &UiSessionBinding) -> bool {
        let Some(target) = self.target.as_ref() else {
            return false;
        };
        let expected_key = HistoryReadKey {
            target: target.clone(),
            query: self.query_value.clone(),
            source: self.source,
        };
        self.active
            && target.binding() == binding
            && !self.loading
            && self.error.is_none()
            && self.loaded_read_key.as_ref() == Some(&expected_key)
    }

    /// 只查询明确 pane 目标的最近一百条；权威刷新不改变填入反馈，空结果也能证明读取完成。
    ///
    /// @author kongweiguang
    fn reload_history(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            self.cancel_read();
            self.entries.clear();
            self.loading = false;
            #[cfg(feature = "right-rail-acceptance")]
            {
                self.loaded_read_key = None;
            }
            return;
        };
        let request_key = HistoryReadKey {
            target,
            query: self.query_value.clone(),
            source: self.source,
        };
        #[cfg(feature = "right-rail-acceptance")]
        if self
            .loaded_read_key
            .as_ref()
            .is_some_and(|loaded| loaded != &request_key)
        {
            self.loaded_read_key = None;
        }
        if !self.active {
            if self
                .read_key
                .as_ref()
                .is_some_and(|current| current != &request_key)
            {
                self.cancel_read();
                self.generation = self.generation.wrapping_add(1).max(1);
                self.loading = false;
                #[cfg(feature = "right-rail-acceptance")]
                {
                    self.loaded_read_key = None;
                }
            }
            return;
        }
        if self.clear_target_in_flight.as_ref() == Some(&request_key.target) {
            self.loading = true;
            cx.notify();
            return;
        }
        if self.read_key.as_ref() == Some(&request_key)
            && self
                .read_cancellation
                .as_ref()
                .is_some_and(|cancellation| !cancellation.is_cancelled())
        {
            return;
        }

        self.cancel_read();
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        let cancellation = CancellationToken::new();
        self.read_cancellation = Some(cancellation.clone());
        self.read_key = Some(request_key.clone());
        #[cfg(feature = "right-rail-acceptance")]
        {
            self.loaded_read_key = None;
        }
        let request = request_key.target.scope().list_request(
            request_key.query.clone(),
            request_key.source,
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
                if !view.response_is_current(generation, &request_key) {
                    return;
                }
                view.loading = false;
                view.read_key = None;
                view.read_cancellation = None;
                match result {
                    Ok(entries) => {
                        view.entries = entries.into_iter().take(HISTORY_LIMIT).collect();
                        view.page =
                            logs_page_window(view.page, view.entries.len(), view.page_size).0;
                        view.status = None;
                        #[cfg(feature = "right-rail-acceptance")]
                        {
                            view.loaded_read_key = Some(request_key.clone());
                        }
                    }
                    Err(error) => {
                        view.error = Some(error.public_message().to_owned());
                        #[cfg(feature = "right-rail-acceptance")]
                        {
                            view.loaded_read_key = None;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 删除时重新校验行与 pane scope，并使此前的成功提示失效；后台权威刷新不会重复作废反馈。
    ///
    /// @author kongweiguang
    fn delete_entry(&mut self, requested_entry_id: &str, cx: &mut Context<Self>) {
        if !self.active || self.loading || self.clear_target_in_flight.is_some() {
            return;
        }
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
        self.fill_feedback.begin_interaction();
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        self.loading = true;
        self.error = None;
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
                    Err(error) => {
                        view.loading = false;
                        view.status = None;
                        view.error = Some(error.public_message().to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 清空使用点击时冻结的 pane scope 并作废旧成功提示；重复点击在异步清理收敛前被合并。
    ///
    /// @author kongweiguang
    fn clear_history(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        if self.loading || self.clear_target_in_flight.is_some() || !self.active {
            return;
        }
        self.fill_feedback.begin_interaction();
        let scope = target.scope().clone();
        let request = scope.clear_request();
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        self.clear_target_in_flight = Some(target.clone());
        self.cancel_read();
        self.generation = self.generation.wrapping_add(1).max(1);
        self.loading = true;
        self.error = None;
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
                if view.clear_target_in_flight.as_ref() != Some(&target) {
                    return;
                }
                view.clear_target_in_flight = None;
                if view.operation_target_is_current(&target) {
                    match result {
                        Ok(count) => {
                            view.status = Some(
                                crate::locale::text(
                                    format!("已清理 {count} 条历史记录"),
                                    format!("Cleared {count} history entries"),
                                )
                                .to_string(),
                            );
                            view.loading = false;
                            view.reload_history(cx);
                        }
                        Err(error) => {
                            view.loading = false;
                            view.status = None;
                            view.error = Some(error.public_message().to_owned());
                        }
                    }
                } else if view.active {
                    view.loading = false;
                    view.reload_history(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 搜索输入和原生验收共用同一变更入口，避免缓存提前更新绕过成功反馈及焦点失效规则。
    ///
    /// @author kongweiguang
    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if self.query_value == query {
            return;
        }
        self.fill_feedback.begin_interaction();
        self.query_value = query;
        self.page = 1;
        self.reload_history(cx);
    }

    /// 改变来源时复位分页并作废旧成功反馈；旧查询会协作取消并检查 generation。
    ///
    /// @author kongweiguang
    fn set_source(&mut self, source: Option<CommandHistorySource>, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        self.fill_feedback.begin_interaction();
        self.source = source;
        self.page = 1;
        self.reload_history(cx);
    }

    /// 只清除搜索词并保留来源筛选；先作废旧反馈并同步缓存，避免订阅重复查询。
    ///
    /// @author kongweiguang
    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.fill_feedback.begin_interaction();
        self.query_value.clear();
        self.page = 1;
        self.query.update(cx, |input, cx| {
            input.set_value(String::new(), window, cx);
        });
        self.reload_history(cx);
        cx.notify();
    }

    /// 分页只调整现有查询结果的本地投影，同时使旧成功提示与焦点票据失效。
    ///
    /// @author kongweiguang
    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let (page, _, _) = logs_page_window(page, self.entries.len(), self.page_size);
        if self.page != page {
            self.fill_feedback.begin_interaction();
            self.page = page;
            cx.notify();
        }
    }

    /// 查询结果按 generation 和完整目标及筛选身份校验；面板是否可见不丢弃当前请求回执。
    ///
    /// @author kongweiguang
    fn response_is_current(&self, generation: u64, request: &HistoryReadKey) -> bool {
        response_matches_request(self.generation, generation, self.read_key.as_ref(), request)
    }

    /// 写操作按已确认的原 scope 完成；同一 pane 搜索变化不能吞掉结果后的刷新。
    ///
    /// @author kongweiguang
    fn operation_target_is_current(&self, target: &LogsTarget) -> bool {
        operation_matches_target(self.active, self.target.as_ref(), target)
    }

    /// 目标或筛选身份变化时取消协作式查询；已确认写操作使用独立的捕获范围。
    ///
    /// @author kongweiguang
    fn cancel_read(&mut self) {
        if let Some(cancellation) = self.read_cancellation.take() {
            cancellation.cancel();
        }
        self.read_key = None;
    }
}

/// 迟到结果只有同时匹配请求代次、目标与筛选才可更新视图；隐藏状态不改变请求有效性。
///
/// @author kongweiguang
fn response_matches_request(
    current_generation: u64,
    request_generation: u64,
    current_request: Option<&HistoryReadKey>,
    request: &HistoryReadKey,
) -> bool {
    current_generation == request_generation && current_request == Some(request)
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

/// 粘贴遵循片段填入的单行约束，避免历史中的换行在终端隐式执行前一行。
///
/// @author kongweiguang
fn history_fill_command_is_valid(command: &str) -> bool {
    let command = command.trim();
    !command.is_empty() && !command.contains(['\r', '\n'])
}

/// 写入完成不等于焦点授权；视图、目标和代次均未变化且结果已收敛才可归还焦点。
///
/// @author kongweiguang
fn history_fill_completion_matches(
    active: bool,
    generation: u64,
    target: Option<&LogsTarget>,
    operation_pending: bool,
    outcome_unknown: bool,
    event: &LogsTerminalFillCompleted,
) -> bool {
    active
        && generation == event.generation
        && !operation_pending
        && !outcome_unknown
        && target.is_some_and(|target| target.binding() == &event.binding)
}

impl Render for LogsView {
    /// 窄栏把刷新/清理移到标题行，搜索输入变化即刷新并保留来源筛选。
    /// 主机名只用于卡片可读性，HostId 留在悬浮与辅助标签；独立右栏使用 main 中性色阶且保留冻结 pane scope。
    /// 历史卡占满剩余高度，按实际卡片高度分页；被动布局不作废操作反馈或焦点票据。
    /// 只有列表负责溢出滚动，分页始终留在底部。
    /// 绘制和分页共享密度几何；刷新只更新原位置反馈并保留待核对的写入结果。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_tool_panel();
        let geometry = HistoryGeometry::from_metrics(theme.metrics);
        let target = self.target.clone();
        let selected_source = self.source;
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
            .debug_selector(|| "rr-logs-source".into())
            .compact()
            .dropdown_caret(true)
            .icon(IconName::ListFilter)
            .w(theme.metrics.control_height * 4.5)
            .h(theme.metrics.control_height)
            .bg(theme.panel)
            .border_color(theme.border)
            .text_color(theme.foreground)
            .child(
                div()
                    .text_size(theme.text_sm)
                    .font_weight(gpui_kit::FontWeight::NORMAL)
                    .child(selected_source_label),
            )
            .accessibility_label(source_accessibility_label.clone())
            .tooltip(source_accessibility_label)
            .dropdown_menu(move |mut menu, _, _| {
                menu = crate::ui_popup::dropdown_menu(menu, theme);
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
            })
            .offset(px(6.));
        let available = matches!(
            self.commands.history_availability(),
            CommandHistoryAvailability::Available
        );
        let loading = self.loading;
        let clearing = self.clear_target_in_flight.is_some();
        let filling_entry = self
            .fill_operation
            .as_ref()
            .map(|operation| operation.entry_id.clone());
        let fill_blocked = !self.active
            || loading
            || clearing
            || self.target.is_none()
            || self.terminal_fill_service.is_none()
            || self.fill_operation.is_some()
            || self.fill_feedback.unknown_target.is_some();
        let fill_status = self
            .fill_feedback
            .status
            .as_ref()
            .map(|status| match status {
                HistoryFillStatus::Confirmed(message) | HistoryFillStatus::Problem(message) => {
                    message.clone()
                }
            });
        let status = self.status.clone();
        let unknown_target = self.fill_feedback.unknown_target.clone();
        let (current_page, start, end) =
            logs_page_window(self.page, self.entries.len(), self.page_size);
        let page_count = self.entries.len().div_ceil(self.page_size).max(1);
        let view = cx.entity().downgrade();
        let card_measurement = canvas(
            move |bounds, _, cx| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                let height = f32::from(bounds.size.height);
                let state = entity.read(cx);
                if state.page_size == history_page_size(height, state.entries.len(), geometry) {
                    return;
                }
                // 延后更新避免在布局阶段重入；保持页码，防止翻页清除反馈后容量增加又跳回上一页。
                cx.defer(move |cx| {
                    entity.update(cx, |state, cx| {
                        let page_size = history_page_size(height, state.entries.len(), geometry);
                        if state.page_size != page_size {
                            state.page_size = page_size;
                            state.page =
                                logs_page_window(state.page, state.entries.len(), page_size).0;
                            cx.notify();
                        }
                    });
                });
            },
            |_, (), _, _| {},
        )
        .absolute()
        .inset_0();
        let page_row_count = end.saturating_sub(start);
        let rows = self.entries[start..end]
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let entry_id = entry.id.clone();
                let copy_entry_id = entry_id.clone();
                let fill_entry_id = entry_id.clone();
                let copy_selector = format!("rr-logs-copy-{entry_id}");
                let fill_selector = format!("rr-logs-fill-{entry_id}");
                let row_selector = format!("rr-logs-row-{entry_id}");
                let command = entry.command.clone();
                let command_tooltip = command.clone();
                let valid_fill_command = history_fill_command_is_valid(&command);
                let fill_label =
                    crate::locale::text("填入终端，不执行", "Fill terminal without executing");
                let fill_tooltip = if valid_fill_command {
                    fill_label.clone()
                } else {
                    crate::locale::text(
                        "仅支持填入单行命令；多行内容可复制",
                        "Only single-line commands can be filled; copy multiline content instead",
                    )
                };
                let copy_label = crate::locale::text("复制命令", "Copy command");
                let context =
                    history_entry_context(entry, target.as_ref(), &self.host_display_names);
                let context_tooltip =
                    history_entry_context_detail(entry, target.as_ref(), &self.host_display_names);
                let created_at = history_timestamp(&entry.created_at);
                let source = source_label(entry.source);
                let metadata = if entry.source == CommandHistorySource::User {
                    created_at.clone()
                } else {
                    format!("{created_at} · {context} · {source}")
                };
                let metadata_text = if entry.source == CommandHistorySource::User {
                    format!("{metadata} · {context}")
                } else {
                    metadata.clone()
                };
                let metadata_tooltip = format!("{metadata} · {context_tooltip}");
                let delete_label = crate::locale::text("删除历史记录", "Delete history entry");
                div()
                    .id(format!("logs-history-row-{entry_id}"))
                    .debug_selector(move || row_selector)
                    .w_full()
                    .h(geometry.row_height)
                    .flex_shrink_0()
                    .px(theme.metrics.panel_padding)
                    .py(theme.metrics.panel_padding / 2.)
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(index + 1 < page_row_count, |row| {
                        row.border_b_1().border_color(theme.border)
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(theme.metrics.gap / 4.)
                            .child(
                                div()
                                    .id(format!("logs-history-command-{entry_id}"))
                                    .w_full()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(theme.text_sm)
                                    .line_height(theme.text_sm * (20. / 14.))
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
                                    .line_height(theme.text_xs * 1.5)
                                    .text_color(theme.muted)
                                    .aria_label(metadata_tooltip.clone())
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(metadata_tooltip.clone()).build(window, cx)
                                    })
                                    .child(metadata_text),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new(format!("logs-history-fill-{entry_id}"))
                                    .debug_selector(move || fill_selector)
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Send)
                                    .accessibility_label(fill_label)
                                    .tooltip(fill_tooltip)
                                    .loading(filling_entry.as_deref() == Some(entry_id.as_str()))
                                    .disabled(fill_blocked || !valid_fill_command)
                                    .on_click(cx.listener(
                                        move |view, _: &ClickEvent, window, cx| {
                                            view.fill_entry(&fill_entry_id, window, cx);
                                        },
                                    )),
                            )
                            .child(
                                Button::new(format!("logs-history-copy-{entry_id}"))
                                    .debug_selector(move || copy_selector)
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Copy)
                                    .accessibility_label(copy_label.clone())
                                    .tooltip(copy_label)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.copy_entry(&copy_entry_id, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("logs-history-delete-{entry_id}"))
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Trash)
                                    .accessibility_label(delete_label.clone())
                                    .tooltip(delete_label)
                                    .disabled(loading || clearing)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.delete_entry(&entry_id, cx);
                                    })),
                            ),
                    )
            });
        let unbound = self.target.is_none();
        let error = self.error.clone();
        let count_label = history_range_label(start, end, self.entries.len());
        let clear_enabled =
            self.target.is_some() && !self.entries.is_empty() && available && !loading && !clearing;
        let show_empty_state = self.entries.is_empty() && !loading && error.is_none() && !unbound;
        let empty_state_message = crate::locale::text("暂无命令历史。", "No command history.");
        let empty_state_content = div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(empty_state(empty_state_message, theme));
        let show_loading_state = self.entries.is_empty() && loading && !unbound;
        let refresh_label = if loading {
            crate::locale::text("正在加载命令历史", "Loading command history")
        } else {
            crate::locale::text("刷新命令历史", "Refresh command history")
        };
        let target_label = target
            .as_ref()
            .map(LogsTarget::label)
            .unwrap_or_else(|| crate::locale::text("当前终端", "Current terminal"));
        let target_detail = target
            .as_ref()
            .map(|target| target.detail().to_owned())
            .unwrap_or_else(|| {
                crate::locale::text("未聚焦终端", "No focused terminal").to_string()
            });
        let target_detail_tooltip = target_detail.clone();
        let mut query_input = Input::new(&self.query).prefix(IconName::Search);
        if !self.query_value.is_empty() {
            query_input = query_input.suffix(
                Button::new("logs-clear-search")
                    .compact()
                    .ghost()
                    .size(theme.metrics.icon_size + theme.metrics.gap)
                    .icon(IconName::X)
                    .accessibility_label(crate::locale::text(
                        "清空命令搜索",
                        "Clear command search",
                    ))
                    .tooltip(crate::locale::text("清空命令搜索", "Clear command search"))
                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                        view.clear_search(window, cx);
                    })),
            );
        }
        let query_input = gpui_kit::Styled::h(
            query_input.flex_1().min_w(px(0.)),
            theme.metrics.control_height,
        )
        .bg(theme.panel)
        .border_color(theme.border)
        .text_color(theme.foreground)
        .text_size(theme.text_sm);
        let mut content = div()
            .id("terminal-logs-view")
            .size_full()
            .flex()
            .flex_col()
            .gap(theme.metrics.gap)
            .p(theme.metrics.panel_padding)
            .overflow_hidden()
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(theme.text_heading)
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(crate::locale::text("命令历史", "Command history")),
            )
            .child(
                div()
                    .id("logs-history-target")
                    .w_full()
                    .h(theme.metrics.control_height)
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.panel)
                                    .text_color(theme.foreground)
                                    .child(target_label),
                            )
                            .child(
                                div()
                                    .id("logs-history-target-detail")
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_family("monospace")
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(target_detail_tooltip.clone())
                                            .build(window, cx)
                                    })
                                    .child(target_detail),
                            ),
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
                                    .size(theme.metrics.control_height)
                                    .loading(loading)
                                    .accessibility_label(refresh_label.clone())
                                    .tooltip(refresh_label)
                                    .disabled(unbound || !available || loading || clearing)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.fill_feedback.begin_interaction();
                                        view.reload_history(cx);
                                    })),
                            )
                            .child(
                                Button::new("logs-clear")
                                    .compact()
                                    .ghost()
                                    .text_color(theme.error)
                                    .icon(IconName::Trash)
                                    .size(theme.metrics.control_height)
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
                                        view.clear_history(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(query_input)
                    .child(source_filter),
            )
            .when_some(status, |content, status| {
                content.child(
                    div()
                        .id("logs-action-status")
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(status),
                )
            })
            .when_some(fill_status, |content, status| {
                content.child(
                    div()
                        .id("logs-fill-status")
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(status),
                )
            })
            .when_some(unknown_target, |content, target| {
                let detail = target.detail().to_owned();
                content.child(
                    div()
                        .id("logs-fill-unknown")
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_size(theme.text_xs)
                        .text_color(theme.error)
                        .child(crate::locale::text(
                            format!("填入结果未知，请检查原终端：{detail}"),
                            format!("Fill result unknown; check the original terminal: {detail}"),
                        ))
                        .child(
                            Button::new("logs-fill-reviewed")
                                .compact()
                                .ghost()
                                .label(crate::locale::text(
                                    "已检查原终端，允许再次填入",
                                    "Original terminal checked; allow another fill",
                                ))
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.fill_feedback.unknown_target = None;
                                    view.fill_feedback.status = Some(HistoryFillStatus::Confirmed(
                                        crate::locale::text(
                                            "已核对原终端",
                                            "Original terminal checked",
                                        )
                                        .to_string(),
                                    ));
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .id("logs-history-card")
                    .debug_selector(|| "rr-logs-card".into())
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .relative()
                    .flex()
                    .flex_col()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .overflow_hidden()
                    .child(card_measurement)
                    .child(
                        div()
                            .id("logs-history-card-header")
                            .w_full()
                            .h(geometry.header_height)
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .px(theme.metrics.panel_padding)
                            .py(theme.metrics.gap / 2.)
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .line_height(theme.text_sm * (20. / 14.))
                                    .text_color(theme.foreground)
                                    .child(crate::locale::text("最近命令", "Recent commands")),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(count_label),
                            ),
                    )
                    .child(
                        div()
                            .id("logs-history-list")
                            .debug_selector(|| "rr-logs-list".into())
                            .w_full()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(rows)
                            .when(show_loading_state, |list| {
                                list.child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .justify_center()
                                        .px_4()
                                        .py_8()
                                        .text_size(theme.text_sm)
                                        .text_color(theme.muted)
                                        .child(crate::locale::text(
                                            "正在加载命令历史…",
                                            "Loading command history…",
                                        )),
                                )
                            })
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
                    )
                    .when(
                        pagination_visible(self.entries.len(), self.page_size),
                        |card| {
                            card.child(
                                div()
                                    .id("logs-history-pagination")
                                    .debug_selector(|| "rr-logs-pagination".into())
                                    .w_full()
                                    .h(geometry.pagination_height)
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .gap_1()
                                    .px_2()
                                    .py(px(6.))
                                    .border_t_1()
                                    .border_color(theme.border)
                                    .text_color(theme.muted)
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(
                                                Button::new("logs-page-previous")
                                                    .debug_selector(|| "rr-logs-previous".into())
                                                    .compact()
                                                    .ghost()
                                                    .icon(IconName::ChevronLeft)
                                                    .size(theme.metrics.control_height)
                                                    .accessibility_label(crate::locale::text(
                                                        "上一页命令历史",
                                                        "Previous history page",
                                                    ))
                                                    .disabled(current_page <= 1)
                                                    .on_click(cx.listener(
                                                        |view, _: &ClickEvent, _, cx| {
                                                            view.set_page(
                                                                view.page.saturating_sub(1).max(1),
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .debug_selector(|| "rr-logs-page-label".into())
                                                    .min_w(theme.metrics.control_height * 2.)
                                                    .h(theme.metrics.control_height)
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .text_center()
                                                    .text_size(theme.text_xs)
                                                    .child(format!(
                                                        "{current_page} / {page_count}"
                                                    )),
                                            )
                                            .child(
                                                Button::new("logs-page-next")
                                                    .debug_selector(|| "rr-logs-next".into())
                                                    .compact()
                                                    .ghost()
                                                    .icon(IconName::ChevronRight)
                                                    .size(theme.metrics.control_height)
                                                    .accessibility_label(crate::locale::text(
                                                        "下一页命令历史",
                                                        "Next history page",
                                                    ))
                                                    .disabled(current_page >= page_count)
                                                    .on_click(cx.listener(
                                                        |view, _: &ClickEvent, _, cx| {
                                                            view.set_page(
                                                                view.page.saturating_add(1),
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            ),
                                    ),
                            )
                        },
                    ),
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
fn pagination_visible(entry_count: usize, page_size: usize) -> bool {
    entry_count > page_size
}

/// 容量取决于卡片实际高度；仅在需要分页时预留底栏，避免高面板仍被固定八条限制。
/// 容量与实际绘制共用密度和 rem 几何，极小窗口至少保留一条并由列表承接溢出。
///
/// @author kongweiguang
fn history_page_size(card_height: f32, total: usize, geometry: HistoryGeometry) -> usize {
    let available = (card_height - 2. - geometry.header_height.as_f32()).max(0.);
    let without_footer = (available / geometry.row_height.as_f32()).floor() as usize;
    let available = if total > without_footer {
        (available - geometry.pagination_height.as_f32()).max(0.)
    } else {
        available
    };
    ((available / geometry.row_height.as_f32()).floor() as usize).clamp(1, HISTORY_LIMIT)
}

/// 列表卡片沿用 main 的页内范围格式；空列表始终显示 `0 / 0`。
///
/// @author kongweiguang
fn history_range_label(start: usize, end: usize, total: usize) -> String {
    if total == 0 || start >= end {
        return "0 / 0".to_owned();
    }

    format!("{}-{} / {total}", start.saturating_add(1), end.min(total))
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
        .py_8()
        .text_size(theme.text_sm)
        .line_height(px(20.))
        .text_color(theme.muted)
        .child(message)
}

/// main 以 Unix 秒存储并用本机月日时分展示；非数字和非法时间原样保留，避免伪造日期。
///
/// @author kongweiguang
fn history_timestamp(value: &str) -> String {
    value
        .parse::<i64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|timestamp| {
            timestamp
                .with_timezone(&chrono::Local)
                .format("%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| value.to_owned())
}

#[cfg(test)]
#[path = "../../../tests/unit/command_logs.rs"]
mod tests;
