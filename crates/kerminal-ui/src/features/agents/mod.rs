//! Agent 右栏的会话管理、目标绑定和上下文预览。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, Window, component::input::InputState,
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

mod actions;
mod custom_launcher_manager;
mod render;

pub(crate) use custom_launcher_manager::{
    CustomAgentLauncherManager, CustomAgentManagerEvent, CustomAgentManagerWriteOutcome,
};

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
    title: Entity<InputState>,
    rename_title: Entity<InputState>,
    title_editor_visible: bool,
    candidate: Option<AgentTargetCandidate>,
    sessions: Vec<AgentSessionView>,
    visible_session_ids: Vec<AgentSessionId>,
    history_mode: AgentHistoryMode,
    current_target_count: usize,
    all_count: usize,
    archived_count: usize,
    rename_target: Option<(AgentSessionId, String)>,
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
        let title = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "会话标题（可留空）",
                "Session title (optional)",
            ))
        });
        let rename_title = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("新会话标题", "New chat title"))
        });
        let mut panel = Self {
            agents,
            executor,
            scope,
            title,
            rename_title,
            title_editor_visible: false,
            candidate: None,
            sessions: Vec::new(),
            visible_session_ids: Vec::new(),
            history_mode: AgentHistoryMode::CurrentTarget,
            current_target_count: 0,
            all_count: 0,
            archived_count: 0,
            rename_target: None,
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

    /// 活动终端变化只更新候选提示，不调用绑定用例，避免普通切 Tab 改写 Agent 首选目标。
    pub fn set_target_candidate(
        &mut self,
        candidate: Option<AgentTargetCandidate>,
        cx: &mut Context<Self>,
    ) {
        if self.candidate == candidate {
            return;
        }
        self.candidate = candidate;
        self.history_mode = AgentHistoryMode::CurrentTarget;
        self.rename_target = None;
        self.reload(cx);
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

    /// 语言切换只更新现有输入实体的占位文案，保留用户草稿、焦点和面板订阅。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.title.update(cx, |input, input_cx| {
            input.set_placeholder(
                crate::locale::text("会话标题（可留空）", "Session title (optional)"),
                window,
                input_cx,
            );
        });
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
