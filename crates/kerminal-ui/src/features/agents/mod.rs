//! Agent 右栏的会话管理、目标绑定和上下文预览。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, Subscription, Window,
    component::input::{InputEvent, InputState},
};
use kerminal_application::{
    ApplicationTaskExecutor, PublicErrorCode,
    agents::{AgentHistoryMode, AgentLauncherCatalog, AgentService, AgentSessionView},
    sessions::TerminalPalette,
};
use kerminal_domain::{
    agents::{AgentLauncherKey, AgentSessionScope, AgentSessionStatus, AgentTargetBinding},
    identity::{AgentSessionId, SessionId},
    sessions::{SessionLifecycle, SessionOwner},
};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "right-rail-acceptance")]
use kerminal_domain::agents::AgentTargetLiveStatus;

mod actions;
mod custom_launcher_manager;
mod render;
#[cfg(feature = "right-rail-acceptance")]
mod right_rail_acceptance;

pub(crate) use custom_launcher_manager::{
    CustomAgentLauncherManager, CustomAgentManagerEvent, CustomAgentManagerWriteOutcome,
};

const AGENT_SEND_HISTORY_LIMIT: usize = 8;
const AGENT_SESSION_TITLE_LIMIT_UTF16: usize = 120;

/// 将标题限制到浏览器 `maxlength` 使用的 UTF-16 单位，且不拆分代理对中的字符。
///
/// @author kongweiguang
pub(super) fn truncate_agent_session_title(value: &str) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|character| {
            let next_units = units + character.len_utf16();
            if next_units > AGENT_SESSION_TITLE_LIMIT_UTF16 {
                return false;
            }
            units = next_units;
            true
        })
        .collect()
}

/// 保存入口与按钮共用同一校验，避免 Enter 绕过空白或长度限制。
///
/// @author kongweiguang
pub(super) fn can_save_agent_session_title(value: &str) -> bool {
    !value.trim().is_empty() && value.encode_utf16().count() <= AGENT_SESSION_TITLE_LIMIT_UTF16
}

/// 单行输入的 Enter 是唯一键盘提交事件；焦点与普通文本变化不触发保存。
///
/// @author kongweiguang
pub(super) fn should_submit_agent_rename(event: &InputEvent) -> bool {
    matches!(event, InputEvent::PressEnter { .. })
}

/// 从终端或命令块捕获的待发送请求；只携带脱敏来源与 opaque ID，不携带命令正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPendingSend {
    pub request_id: String,
    pub source_label: String,
}

/// Root 完成现有安全预览流程后交给面板的瞬时正文；不能持久化或写入历史 metadata。
#[derive(Clone, Eq, PartialEq)]
pub struct AgentPendingSendPreview {
    pub agent_session_id: AgentSessionId,
    pub byte_length: usize,
    pub expires_at: String,
    pub preview_id: String,
    pub redacted: bool,
    pub request_id: String,
    pub text: String,
    pub truncated: bool,
}

/// 最近发送记录不保留正文，只提供可供右栏列表展示的审计 metadata。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSendHistoryMetadata {
    pub agent_session_id: AgentSessionId,
    pub byte_length: usize,
    pub created_at: String,
    pub outcome: AgentSendOutcome,
    pub request_id: String,
    pub source_label: String,
}

/// 发送历史只展示已知结果，不把请求受理误报为 Agent 已完成处理。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSendOutcome {
    Sent,
    Queued,
    Cancelled,
    Expired,
    Failed,
}

#[cfg(test)]
#[path = "../../../tests/unit/agent_selection.rs"]
mod selection_tests;

/// 外部导航只能选择列表已核验的现有非归档 Agent，不承担创建、恢复或重绑职责。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSelectionError {
    /// AgentService 列表仍在刷新，当前投影不能证明目标状态。
    ListRefreshing,
    /// 最近一次 AgentService 列表读取失败，旧投影不能用于外部选择。
    ListUnavailable,
    /// 最近一次成功列表中不存在该确切 AgentSessionId。
    NotFound,
    /// 该 Agent 会话已归档，不能通过导航入口重新激活。
    Archived,
}

/// Root 的显式目标候选只用于新建或用户点击“绑定”，Tab 变化不会自动写入 Agent 状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTargetCandidate {
    pub binding: AgentTargetBinding,
    pub label: String,
}

/// 将右栏意图交给 Root；目标绑定通知只表示已提交，不复制面板已保存的目标快照。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentPanelEvent {
    Selected {
        agent_session_id: Option<AgentSessionId>,
        terminal_session_id: Option<SessionId>,
    },
    /// 仅新建入口声明一次焦点意图；Root 捕获当时焦点并在异步完成时防止抢占用户的新选择。
    NewTerminalFocusStarted {
        request_generation: u64,
    },
    /// 新建回执给出确切 Running PTY；必须与 Started 同代次配对，刷新和恢复不会发出此事件。
    NewTerminalFocusReady {
        request_generation: u64,
        agent_session_id: AgentSessionId,
        terminal_session_id: SessionId,
    },
    /// 新建失败、取消、选择基线变化或没有可交互 PTY 时放弃这次焦点意图。
    NewTerminalFocusCancelled {
        request_generation: u64,
    },
    /// 用户遇到 Launcher 配置问题时请求打开全局偏好设置，不改变会话或启动选择。
    OpenAgentLauncherSettings,
    /// 从启动器下拉进入自定义定义管理；Root 负责全窗口弹层和已确认设置快照。
    OpenCustomAgentManager,
    /// 请求 Root 通过现有 preview/send service 发送已经明确确认的预览。
    PendingSendConfirmed {
        agent_session_id: AgentSessionId,
        request_id: String,
        preview_id: String,
    },
    /// 用户取消待发送命令；Root 负责消耗原请求并记录取消 metadata。
    PendingSendCancelled {
        request_id: String,
    },
    TargetBindingChanged,
}

/// 仅当创建回执明确持有同一 Agent 的 Running PTY 时才允许自动获得一次焦点。
///
/// @author kongweiguang
pub(super) fn new_terminal_focus_target(
    view: &AgentSessionView,
) -> Option<(AgentSessionId, SessionId)> {
    if view.session.status != AgentSessionStatus::Active || view.resume_recovery.is_some() {
        return None;
    }
    let agent_session_id = view.session.agent_session_id.clone();
    let terminal = view.terminal.as_ref()?;
    if terminal.lifecycle != SessionLifecycle::Running
        || !matches!(
            &terminal.owner,
            SessionOwner::Agent { agent_session_id: owner_id } if owner_id == &agent_session_id
        )
    {
        return None;
    }
    Some((agent_session_id, terminal.session_id.clone()))
}

/// 改名入口捕获条目身份与预期标题作为同一 CAS 草稿基线，不从后来的选择补取字段。
///
/// @author kongweiguang
struct AgentRenameTarget {
    id: AgentSessionId,
    expected_title: String,
}

/// 确认框绑定打开时的会话、筛选和候选目标；generation 阻止旧回执关闭后来打开的弹框。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AgentArchiveConfirmationOwner {
    pub(super) id: AgentSessionId,
    pub(super) expected_title: String,
    pub(super) mode: AgentHistoryMode,
    pub(super) scope: AgentSessionScope,
    pub(super) candidate: Option<AgentTargetBinding>,
    pub(super) generation: u64,
}

impl AgentArchiveConfirmationOwner {
    /// 只有所有捕获上下文和代次都相同时才认作同一个确认框 owner。
    ///
    /// @author kongweiguang
    pub(super) fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

/// 生命周期请求捕获身份、选择基线和显式恢复时的旧 PTY 身份，异步完成时不补取后来状态。
///
/// @author kongweiguang
struct AgentLifecycleOperation {
    target_id: AgentSessionId,
    selected_before: Option<AgentSessionId>,
    terminal_feedback_reset: AgentTerminalFeedbackReset,
    pending: (&'static str, &'static str),
    success: (&'static str, &'static str),
    cancellable: bool,
}

/// 真实 runtime 故障只在显式恢复成功且返回新 Running PTY 后清除；刷新和核对不拥有此权限。
///
/// @author kongweiguang
enum AgentTerminalFeedbackReset {
    Never,
    OnNewRunningTerminal {
        previous_terminal_session_id: Option<SessionId>,
    },
}

/// 终端问题与 Agent 创建/继续等业务回执分开保存，避免轮询误清用户操作结果。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AgentTerminalFeedback {
    agent_session_id: AgentSessionId,
    message: String,
    source: AgentTerminalFeedbackSource,
}

/// 只有可被同一 Agent 的 Exited 投影解释的拒绝可自动收敛；运行错误和不确定结果持久保留。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentTerminalFeedbackSource {
    RuntimeFailure,
    OperationFailure(PublicErrorCode),
}

impl AgentTerminalFeedbackSource {
    /// 不按本地化错误文本推断生命周期；只有确定的冲突/失效请求可由 Exited 读回收敛。
    ///
    /// @author kongweiguang
    pub(super) const fn clears_after_confirmed_exit(self) -> bool {
        matches!(
            self,
            Self::OperationFailure(PublicErrorCode::Conflict | PublicErrorCode::NotFound)
        )
    }
}

/// Agent 右栏只保存可见投影和草稿；运行进程及终端仍由共享 application services 管理。
pub struct AgentsPanel {
    agents: Arc<dyn AgentService>,
    executor: ApplicationTaskExecutor,
    scope: AgentSessionScope,
    rename_title: Entity<InputState>,
    _rename_subscription: Subscription,
    candidate: Option<AgentTargetCandidate>,
    target_summary: Option<String>,
    sessions: Vec<AgentSessionView>,
    visible_session_ids: Vec<AgentSessionId>,
    history_mode: AgentHistoryMode,
    current_target_count: usize,
    all_count: usize,
    archived_count: usize,
    rename_target: Option<(AgentSessionId, String)>,
    archive_confirmation_owner: Option<AgentArchiveConfirmationOwner>,
    archive_confirmation_generation: u64,
    archive_confirmation_error: Option<String>,
    selected_agent: Option<AgentSessionId>,
    launcher_catalog: Option<AgentLauncherCatalog>,
    launcher_key: Option<AgentLauncherKey>,
    launcher_catalog_loading: bool,
    launcher_selection_busy: bool,
    launcher_load_error: Option<String>,
    launcher_selection_error: Option<String>,
    launcher_generation: u64,
    context_preview: Option<String>,
    context_preview_generation: u64,
    context_preview_loading: bool,
    theme_mode: kerminal_application::preferences::ThemeMode,
    terminal_palette: Option<TerminalPalette>,
    busy: bool,
    operation_cancellation: Option<CancellationToken>,
    loading: bool,
    sessions_verified: bool,
    status: Option<String>,
    terminal_feedback: Vec<AgentTerminalFeedback>,
    launcher_setup_prompt: bool,
    generation: u64,
    /// 只标识普通新建与历史新建的焦点请求；列表刷新和显式恢复使用独立语义。
    creation_focus_generation: u64,
    active_creation_focus_generation: Option<u64>,
    pending_send: Option<AgentPendingSend>,
    pending_send_preview: Option<AgentPendingSendPreview>,
    send_history: Vec<AgentSendHistoryMetadata>,
}

impl EventEmitter<AgentPanelEvent> for AgentsPanel {}

impl AgentsPanel {
    /// 接收根窗口已装配的 Agent 用例与任务执行器，构造时不自动创建 CLI 或写入目标绑定。
    pub fn new(
        agents: Arc<dyn AgentService>,
        executor: ApplicationTaskExecutor,
        scope: AgentSessionScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rename_title = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("新会话标题", "New chat title"))
        });
        let rename_subscription = cx.subscribe_in(
            &rename_title,
            window,
            |panel, input_state, event, window, cx| {
                if should_submit_agent_rename(event) {
                    panel.save_agent_title(cx);
                } else if matches!(event, InputEvent::Change) {
                    let value = input_state.read(cx).value();
                    let bounded = truncate_agent_session_title(value.as_ref());
                    if bounded != value.as_ref() {
                        input_state.update(cx, |input, input_cx| {
                            input.set_value(bounded, window, input_cx);
                        });
                    }
                    cx.notify();
                }
            },
        );
        let mut panel = Self {
            agents,
            executor,
            scope,
            rename_title,
            _rename_subscription: rename_subscription,
            candidate: None,
            target_summary: None,
            sessions: Vec::new(),
            visible_session_ids: Vec::new(),
            history_mode: AgentHistoryMode::CurrentTarget,
            current_target_count: 0,
            all_count: 0,
            archived_count: 0,
            rename_target: None,
            archive_confirmation_owner: None,
            archive_confirmation_generation: 0,
            archive_confirmation_error: None,
            selected_agent: None,
            launcher_catalog: None,
            launcher_key: None,
            launcher_catalog_loading: false,
            launcher_selection_busy: false,
            launcher_load_error: None,
            launcher_selection_error: None,
            launcher_generation: 0,
            context_preview: None,
            context_preview_generation: 0,
            context_preview_loading: false,
            theme_mode: kerminal_application::preferences::ThemeMode::System,
            terminal_palette: None,
            busy: false,
            operation_cancellation: None,
            loading: false,
            sessions_verified: false,
            status: None,
            terminal_feedback: Vec::new(),
            launcher_setup_prompt: false,
            generation: 0,
            creation_focus_generation: 0,
            active_creation_focus_generation: None,
            pending_send: None,
            pending_send_preview: None,
            send_history: Vec::new(),
        };
        panel.reload(cx);
        panel.reload_launcher_catalog(cx);
        panel
    }

    /// 保存 Root 当前色盘供新建、历史派生和恢复使用，避免 PTY 启动早于异步主题同步。
    ///
    /// @author kongweiguang
    pub fn set_terminal_palette(&mut self, palette: TerminalPalette, cx: &mut Context<Self>) {
        if self.terminal_palette != Some(palette) {
            self.terminal_palette = Some(palette);
            cx.notify();
        }
    }

    /// Root 将 command capture、短期 preview 与无正文发送历史投影到面板；只接受配对 request ID。
    ///
    /// @author kongweiguang
    pub fn set_pending_send_state(
        &mut self,
        pending: Option<AgentPendingSend>,
        preview: Option<AgentPendingSendPreview>,
        mut history: Vec<AgentSendHistoryMetadata>,
        cx: &mut Context<Self>,
    ) {
        history.truncate(AGENT_SEND_HISTORY_LIMIT);
        let preview = preview.filter(|preview| {
            pending
                .as_ref()
                .is_some_and(|pending| pending.request_id == preview.request_id)
        });
        if self.pending_send == pending
            && self.pending_send_preview == preview
            && self.send_history == history
        {
            return;
        }
        self.pending_send = pending;
        self.pending_send_preview = preview;
        self.send_history = history;
        cx.notify();
    }

    /// 根窗口同步新建会话的归属范围；范围改变时作废旧预览请求，不改写既有会话归属。
    pub fn set_scope(&mut self, scope: AgentSessionScope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.invalidate_context_preview();
        self.scope = scope;
        self.history_mode = AgentHistoryMode::CurrentTarget;
        self.rename_target = None;
        self.reload(cx);
        cx.notify();
    }

    /// 候选变化重置其可见摘要并刷新历史，不调用绑定用例，避免普通切 Tab 改写 Agent 首选目标。
    pub fn set_target_candidate(
        &mut self,
        candidate: Option<AgentTargetCandidate>,
        cx: &mut Context<Self>,
    ) {
        if self.candidate == candidate {
            return;
        }
        self.candidate = candidate;
        self.target_summary = None;
        self.history_mode = AgentHistoryMode::CurrentTarget;
        self.rename_target = None;
        self.reload(cx);
        cx.notify();
    }

    /// 更新只读目标摘要；Root 负责解析 tab 与终端数量，摘要变化不触发历史查询或绑定。
    ///
    /// @author kongweiguang
    pub fn set_target_summary(&mut self, summary: Option<String>, cx: &mut Context<Self>) {
        let summary = summary
            .map(|summary| summary.trim().to_owned())
            .filter(|summary| !summary.is_empty());
        if self.target_summary == summary {
            return;
        }
        self.target_summary = summary;
        cx.notify();
    }

    /// 系统或用户主题变化仅刷新语义令牌，不重置当前 Agent 草稿和选择。
    pub fn set_theme_mode(
        &mut self,
        mode: kerminal_application::preferences::ThemeMode,
        cx: &mut Context<Self>,
    ) {
        self.theme_mode = mode;
        cx.notify();
    }

    /// 语言切换只更新当前改名草稿的占位文案，避免重建输入实体并丢失焦点。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rename_title.update(cx, |input, input_cx| {
            input.set_placeholder(
                crate::locale::text("新会话标题", "New chat title"),
                window,
                input_cx,
            );
        });
        cx.notify();
    }

    /// 供根工作台在会话归档或运行态关闭后刷新服务投影，避免外层复制列表状态机。
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    /// 仅在 Root 收到 Preferences CAS 成功回执后重读 catalog，不触发第二次 launcher 选择写入。
    ///
    /// @author kongweiguang
    pub(crate) fn refresh_launcher_catalog_after_custom_settings_saved(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.launcher_selection_error = None;
        self.launcher_load_error = None;
        self.reload_launcher_catalog(cx);
    }

    /// 仅选择最近一次成功列表中的确切非归档会话；校验失败时保留当前选择和预览。
    ///
    /// @author kongweiguang
    pub fn select_existing_agent(
        &mut self,
        agent_session_id: AgentSessionId,
        cx: &mut Context<Self>,
    ) -> Result<(), AgentSelectionError> {
        let result = validate_existing_agent_selection(
            &self.sessions,
            self.loading,
            self.sessions_verified,
            &agent_session_id,
        );
        if let Err(error) = result {
            self.status = Some(
                crate::locale::text(
                    match error {
                        AgentSelectionError::ListRefreshing => {
                            "Agent 列表正在刷新，请稍后重试。"
                        }
                        AgentSelectionError::ListUnavailable => {
                            "Agent 列表尚未成功读取，请刷新后重试。"
                        }
                        AgentSelectionError::NotFound => {
                            "指定 Agent 会话已不存在，请刷新列表。"
                        }
                        AgentSelectionError::Archived => {
                            "指定 Agent 会话已归档，不能从导航入口重新激活。"
                        }
                    },
                    match error {
                        AgentSelectionError::ListRefreshing => {
                            "The Agent list is refreshing. Try again shortly."
                        }
                        AgentSelectionError::ListUnavailable => {
                            "The Agent list is unavailable. Refresh and try again."
                        }
                        AgentSelectionError::NotFound => {
                            "The requested Agent session no longer exists. Refresh the list."
                        }
                        AgentSelectionError::Archived => {
                            "The requested Agent session is archived and cannot be reactivated from navigation."
                        }
                    },
                )
                .to_string(),
            );
            cx.notify();
            return Err(error);
        }

        self.select_agent(agent_session_id, cx);
        Ok(())
    }

    /// 标签切回时从原面板恢复选择，不重新挑选历史项或改变 CLI 首选目标。
    ///
    /// @author kongweiguang
    pub fn selected_agent_session_id(&self) -> Option<AgentSessionId> {
        self.selected_agent.clone()
    }

    /// 返回真实读取错误；已有会话检查持久化首选，无选中会话时才检查候选，不写入目标绑定。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn right_rail_acceptance_agent_ready_for_fixture(
        &self,
        fixture_target: &AgentTargetBinding,
    ) -> Result<bool, String> {
        if self.loading || self.launcher_catalog_loading || self.launcher_selection_busy {
            return Ok(false);
        }
        if let Some(error) = self.launcher_load_error.as_ref() {
            return Err(error.clone());
        }
        if let Some(error) = self.launcher_selection_error.as_ref() {
            return Err(error.clone());
        }
        if !self.sessions_verified {
            return Err(self.status.clone().unwrap_or_else(|| {
                crate::locale::text(
                    "Agent 会话列表尚未成功读取。",
                    "Agent session list is unavailable.",
                )
                .to_string()
            }));
        }
        if self.launcher_catalog.is_none() {
            return Ok(false);
        }
        let Some(fixture_host_id) = fixture_target
            .target_ref
            .as_deref()
            .filter(|host_id| !host_id.trim().is_empty())
        else {
            return Ok(false);
        };
        if fixture_target.target_kind.as_deref() != Some("ssh")
            || fixture_target.live_status != AgentTargetLiveStatus::Ready
            || fixture_target
                .preferred_session_id()
                .ok()
                .flatten()
                .is_none()
        {
            return Ok(false);
        }
        let matches_fixture_host = |binding: &AgentTargetBinding| {
            binding.target_kind.as_deref() == Some("ssh")
                && binding.target_ref.as_deref() == Some(fixture_host_id)
                && binding.live_status == AgentTargetLiveStatus::Ready
                && binding.preferred_session_id().ok().flatten().is_some()
        };
        if let Some(view) = self.selected_view() {
            return Ok(view
                .session
                .target
                .as_ref()
                .is_some_and(matches_fixture_host));
        }
        Ok(self
            .candidate
            .as_ref()
            .is_some_and(|candidate| matches_fixture_host(&candidate.binding)))
    }

    /// 返回当前 Agent-owned terminal id；没有活进程时不伪造 canvas 目标。
    pub fn selected_terminal_session_id(&self) -> Option<SessionId> {
        self.selected_view()?
            .terminal
            .as_ref()
            .map(|terminal| terminal.session_id.clone())
    }

    /// 作废旧选择的预览响应，并结束预览专属忙碌状态，避免迟到结果覆盖新会话。
    ///
    /// @author kongweiguang
    fn invalidate_context_preview(&mut self) {
        self.context_preview_generation = self.context_preview_generation.wrapping_add(1).max(1);
        self.context_preview = None;
        if self.context_preview_loading {
            self.context_preview_loading = false;
            self.busy = false;
            self.status = None;
        }
    }

    /// 只从本次 service 投影中解析选择，不以索引或当前 Tab 猜测 Agent 会话。
    fn selected_view(&self) -> Option<&AgentSessionView> {
        let id = self.selected_agent.as_ref()?;
        self.sessions
            .iter()
            .find(|view| &view.session.agent_session_id == id)
    }

    /// 终端反馈只属于产生它的稳定 Agent 身份，切换历史项不会把旧错误串到新会话。
    ///
    /// @author kongweiguang
    fn visible_terminal_feedback(&self) -> Option<&AgentTerminalFeedback> {
        let selected_agent = self.selected_agent.as_ref()?;
        self.terminal_feedback
            .iter()
            .find(|feedback| &feedback.agent_session_id == selected_agent)
    }

    /// 每个稳定 Agent 身份最多保留一条终端反馈；较窄的退出拒绝不能覆盖真实或未知故障。
    ///
    /// @author kongweiguang
    fn record_terminal_feedback(&mut self, feedback: AgentTerminalFeedback) {
        if let Some(current) = self
            .terminal_feedback
            .iter_mut()
            .find(|current| current.agent_session_id == feedback.agent_session_id)
        {
            if !terminal_feedback_should_replace(current, &feedback) {
                return;
            }
            *current = feedback;
        } else {
            self.terminal_feedback.push(feedback);
        }
    }
}

/// 较窄的生命周期拒绝不能抹掉同一 Agent 上更强的 runtime 或未知结果反馈。
///
/// @author kongweiguang
fn terminal_feedback_should_replace(
    current: &AgentTerminalFeedback,
    incoming: &AgentTerminalFeedback,
) -> bool {
    let current_is_runtime = current.source == AgentTerminalFeedbackSource::RuntimeFailure;
    let incoming_is_narrow = incoming.source.clears_after_confirmed_exit();
    let current_is_persistent = !current.source.clears_after_confirmed_exit();
    !current_is_runtime && !(incoming_is_narrow && current_is_persistent)
}

/// 导航入口只依据最近成功读取的 AgentService 列表作准入判断，禁止从旧缓存推断状态。
///
/// @author kongweiguang
fn validate_existing_agent_selection(
    sessions: &[AgentSessionView],
    loading: bool,
    sessions_verified: bool,
    requested_id: &AgentSessionId,
) -> Result<(), AgentSelectionError> {
    if loading {
        return Err(AgentSelectionError::ListRefreshing);
    }
    if !sessions_verified {
        return Err(AgentSelectionError::ListUnavailable);
    }
    let Some(session) = sessions
        .iter()
        .find(|view| &view.session.agent_session_id == requested_id)
    else {
        return Err(AgentSelectionError::NotFound);
    };
    if session.session.status == AgentSessionStatus::Archived {
        return Err(AgentSelectionError::Archived);
    }
    Ok(())
}
