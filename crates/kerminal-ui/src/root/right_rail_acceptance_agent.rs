//! Agent 右栏生产流程的隔离配置与状态回读桥。
//!
//! @author kongweiguang

use std::path::PathBuf;

use gpui_kit::{Context, Window};
use kerminal_application::{
    AppError, AppResult, PreferencesService,
    agents::{AgentHistoryMode, AgentSessionView},
    preferences::{
        ConfigurationValidationScope, ConfigurationValidationService, CustomAgentDefinition,
        settings::ToolRailToolId,
    },
    sessions::UiSessionBinding,
    workspace::{WorkspaceSessionSnapshot, WorkspaceSessionTab},
};
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{AgentSessionId, HostId, PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionLifecycle, SessionOwner, SessionTarget},
    workspace::SplitDirection,
};
use kerminal_terminal::snapshot::RenderSnapshot;
use uuid::Uuid;

use crate::features::workspace_panes::WorkspacePaneEvent;
use crate::features::workspace_tabs::WorkspaceTabEvent;
use crate::shell_arguments::quote_shell_argument;

use super::{AppServices, RootView, TerminalTargetRequest};

/// 隔离夹具的启动形态；生命周期闭环沿用一次性退出命令，连续性场景改用可同步多阶段脚本。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub enum AgentAcceptanceFixture {
    /// 输出唯一标记后自然退出，用于验证创建、继续、改名、输出与归档闭环。
    Exiting,
    /// 常驻本地脚本，按 stdin 逐步回应，用于验证隐藏 Tab 期间 PTY 仍在运行。
    Staged(AgentAcceptanceStagedFixture),
}

impl AgentAcceptanceFixture {
    /// 默认生命周期夹具不需要额外文件，保留原验收路径不变。
    ///
    /// @author kongweiguang
    pub fn exiting() -> Self {
        Self::Exiting
    }

    /// 连续性夹具只接受由验收方生成的独占触发路径，不拼接用户命令或凭据。
    ///
    /// @author kongweiguang
    pub fn staged(stage_path: PathBuf) -> Self {
        Self::Staged(AgentAcceptanceStagedFixture::new(stage_path))
    }
}

/// 多阶段夹具的触发文件位置；驱动方负责原子创建，UI 只把它作为 PowerShell 单引号字面量嵌入命令。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub struct AgentAcceptanceStagedFixture {
    stage_path: PathBuf,
}

impl AgentAcceptanceStagedFixture {
    /// 固定本轮独立的触发文件路径，命令构造只读取该路径，不再生成或覆盖脚本文件。
    ///
    /// @author kongweiguang
    pub fn new(stage_path: PathBuf) -> Self {
        Self { stage_path }
    }
}

/// 只读的右栏 Agent 面板连续性投影；不复制面板内部状态，也不触发重新加载。
///
/// @author kongweiguang
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentContinuityPanelState {
    panel_tab_id: Option<TabId>,
    panel_entity_id: Option<u64>,
    selected_agent_session_id: Option<AgentSessionId>,
    selected_terminal_session_id: Option<SessionId>,
    terminal_lifecycle: Option<SessionLifecycle>,
    target_generation: Option<u64>,
    target_terminal_session_id: Option<SessionId>,
    session_count: usize,
    /// 当前历史过滤；原生 activity 前后必须保持不变。
    history_mode: AgentHistoryMode,
}

impl AgentContinuityPanelState {
    /// Tab 归属用于证明切回时挂回的是原标签面板，而不是新建的替代实体。
    ///
    /// @author kongweiguang
    pub fn panel_tab_id(&self) -> Option<&TabId> {
        self.panel_tab_id.as_ref()
    }

    /// GPUI 实体身份用于排除换挂期间意外重建 AgentsPanel。
    ///
    /// @author kongweiguang
    pub fn panel_entity_id(&self) -> Option<u64> {
        self.panel_entity_id
    }

    /// 当前选中的 Agent 必须在切回后保持不变，且其它 Tab 不应显示同一选择。
    ///
    /// @author kongweiguang
    pub fn selected_agent_session_id(&self) -> Option<&AgentSessionId> {
        self.selected_agent_session_id.as_ref()
    }

    /// Agent 自身 PTY 身份与应用侧 `SessionId` 分开校验，避免把用户终端误当成 Agent 终端。
    ///
    /// @author kongweiguang
    pub fn selected_terminal_session_id(&self) -> Option<&SessionId> {
        self.selected_terminal_session_id.as_ref()
    }

    /// 面板绘制的生命周期来自真实 watcher 投影；切回后仍需是 Running。
    ///
    /// @author kongweiguang
    pub fn terminal_lifecycle(&self) -> Option<SessionLifecycle> {
        self.terminal_lifecycle
    }

    /// 绑定代次只允许由显式动作推进，普通 Tab 切换前后必须一致。
    ///
    /// @author kongweiguang
    pub fn target_generation(&self) -> Option<u64> {
        self.target_generation
    }

    /// 首选目标终端身份与绑定代次一起锁定，防止静默改投其它终端。
    ///
    /// @author kongweiguang
    pub fn target_terminal_session_id(&self) -> Option<&SessionId> {
        self.target_terminal_session_id.as_ref()
    }

    /// 历史列表数量用于确认切回没有触发隐式重读或新增会话。
    ///
    /// @author kongweiguang
    pub fn session_count(&self) -> usize {
        self.session_count
    }

    /// 当前历史过滤；原生 activity 前后比较，防止窗口切换隐式重置筛选。
    ///
    /// @author kongweiguang
    pub fn history_mode(&self) -> AgentHistoryMode {
        self.history_mode
    }
}

/// 一次隔离验收使用的 launcher 身份和终端输出标记，不包含任何用户配置内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentAcceptanceSetup {
    launcher_id: String,
    output_marker: String,
}

impl AgentAcceptanceSetup {
    /// Exposes only the generated launcher identity needed by the desktop acceptance driver.
    ///
    /// @author kongweiguang
    pub fn launcher_id(&self) -> &str {
        &self.launcher_id
    }

    /// Exposes the per-run output marker without making setup state mutable across crate boundaries.
    ///
    /// @author kongweiguang
    pub fn output_marker(&self) -> &str {
        &self.output_marker
    }
}

/// 验收目标只区分 SSH fixture 与本地模板终端；两条路径都复用同一生产面板、会话和绑定校验。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
enum AgentAcceptanceBindingScope<'a> {
    Ssh(&'a HostId),
    Local,
}

/// 单个 pane 的运行态回收证据。`pane_present` 与 `layout_present` 分开报告，因为生产关闭路径
/// 先提交布局候选、后完成 close 回执；把两者合并成“可见 pane 数量”会让布局移除冒充资源回收。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ContinuityPanePresence {
    pane_present: bool,
    session_bound: bool,
    layout_present: bool,
}

impl ContinuityPanePresence {
    /// 只有运行态 pane 与 session 映射都消失，且布局不再引用该 pane，才算真实移除；
    /// 布局仍在而运行态先消失同样是未收敛状态，不能只看单一侧。
    ///
    /// @author kongweiguang
    fn fully_removed(self) -> bool {
        !self.pane_present && !self.session_bound && !self.layout_present
    }
}

/// 从已提交快照与运行态读取存在性；纯函数形式让“布局移除但运行态仍持有”等竞态可用单测固定，
/// 不依赖真实窗口或关闭任务。缺失 controller 快照按布局不存在处理，但运行态仍会独立报告。
///
/// @author kongweiguang
fn continuity_pane_presence(
    snapshot: Option<&WorkspaceSessionSnapshot>,
    runtime: &super::workspace_runtime::WorkspaceRuntime,
    tab_id: &TabId,
    pane_id: &PaneId,
    session_id: &SessionId,
) -> ContinuityPanePresence {
    let pane_present = runtime.pane(pane_id).is_some();
    let session_bound = runtime.pane_for_session(session_id) == Some(pane_id);
    let layout_present = snapshot.is_some_and(|snapshot| {
        snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)
            .and_then(WorkspaceSessionTab::terminal_layout)
            .is_some_and(|layout| layout.contains_pane(pane_id))
    });
    ContinuityPanePresence {
        pane_present,
        session_bound,
        layout_present,
    }
}

impl RootView {
    /// 原生双窗口验收临时关闭隐藏窗口活动态覆盖，确保失焦路径使用真实 OS 状态；恢复后沿用隔离验收的隐藏投影。
    ///
    /// @author kongweiguang
    pub fn set_right_rail_acceptance_activity_override(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.right_rail_acceptance_active_override = enabled;
        self.sync_tool_projection_before_first_draw(window, cx);
        cx.notify();
    }

    /// Captures only when the validated fixture pane is also owned by Root's currently visible Terminal tab.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_fixture_tab_id(&self, host_id: &HostId) -> Result<TabId, String> {
        self.right_rail_acceptance_binding_tab_id(AgentAcceptanceBindingScope::Ssh(host_id))
    }

    /// 本地连续性入口只接受活动 pane 为真实 Local 会话且位于可见 Terminal Tab；
    /// 未就绪时返回 `Ok(None)`，让调用方继续等待生产会话创建，而不是把暂时缺失误判为失败。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_local_terminal_ready_tab_id(
        &self,
    ) -> Result<Option<TabId>, String> {
        let Some(binding) = self.right_rail_acceptance_local_fixture_binding()? else {
            return Ok(None);
        };
        let SessionOwner::Ui { tab_id, .. } = binding.owner else {
            return Ok(None);
        };
        if self.visible_workspace_tab_id().as_ref() != Some(&tab_id)
            || !matches!(self.center_panel, super::CenterPanel::Terminal)
        {
            return Ok(None);
        }
        Ok(Some(tab_id))
    }

    /// SSH 与本地路径共享同一“可见 Terminal Tab + 精确 owner”门禁，避免两套验收各自解释可见性。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_binding_tab_id(
        &self,
        scope: AgentAcceptanceBindingScope<'_>,
    ) -> Result<TabId, String> {
        let binding = self
            .right_rail_acceptance_acceptance_binding(scope)?
            .ok_or_else(|| {
                match scope {
                    AgentAcceptanceBindingScope::Ssh(_) => {
                        "Agent acceptance has no active fixture SSH pane"
                    }
                    AgentAcceptanceBindingScope::Local => {
                        "Agent acceptance has no active local terminal pane"
                    }
                }
                .to_owned()
            })?;
        let tab_id = match binding.owner {
            SessionOwner::Ui { tab_id, .. } => tab_id,
            _ => return Err("Agent acceptance fixture is not owned by a workspace tab".to_owned()),
        };
        if self.visible_workspace_tab_id().as_ref() != Some(&tab_id)
            || !matches!(self.center_panel, super::CenterPanel::Terminal)
        {
            return Err(
                "Agent acceptance fixture pane is not in the visible Terminal tab".to_owned(),
            );
        }
        Ok(tab_id)
    }

    /// 本地副本沿用 SSH fixture 相同的 pending modal、Ready pane、精确 owner 和 renderer snapshot 检查；
    /// 单独实现是因为 SSH 门禁属于另一验收文件，本任务不修改该文件的通用逻辑。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_local_fixture_binding(
        &self,
    ) -> Result<Option<UiSessionBinding>, String> {
        if self.connection_prompt_modal_open {
            return Err("acceptance external credential prompt is pending".to_owned());
        }
        let Some(session_id) = self.active_session_id() else {
            return Ok(None);
        };
        let Some(pane_id) = self
            .workspace_runtime
            .pane_for_session(&session_id)
            .cloned()
        else {
            return Ok(None);
        };
        let Some(pane) = self.workspace_runtime.pane(&pane_id) else {
            return Ok(None);
        };
        if pane.phase != super::RuntimePanePhase::Ready {
            return Ok(None);
        }
        if !matches!(pane.target.as_ref(), Some(SessionTarget::Local(_))) {
            return Err("active fixture pane is not a local terminal".to_owned());
        }
        let Some(binding) = self.terminal_view_bindings.get(&session_id) else {
            return Ok(None);
        };
        let expected_owner = SessionOwner::Ui {
            tab_id: pane.tab_id.clone(),
            pane_id,
        };
        if binding.session_id != session_id || binding.owner != expected_owner {
            return Err("active fixture pane has a stale terminal binding".to_owned());
        }
        if self.terminal_snapshot_for_binding(binding).is_none() {
            return Ok(None);
        }
        Ok(Some(binding.clone()))
    }

    /// 两种目标只在绑定来源上不同，后续候选与 panel 校验保持同一路径。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_acceptance_binding(
        &self,
        scope: AgentAcceptanceBindingScope<'_>,
    ) -> Result<Option<UiSessionBinding>, String> {
        match scope {
            AgentAcceptanceBindingScope::Ssh(host_id) => {
                self.right_rail_acceptance_fixture_binding(host_id)
            }
            AgentAcceptanceBindingScope::Local => {
                self.right_rail_acceptance_local_fixture_binding()
            }
        }
    }

    /// Restores only the captured TabId through the normal reducer and verifies Root's visible selection afterward.
    ///
    /// @author kongweiguang
    pub fn restore_right_rail_acceptance_fixture_tab(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.visible_workspace_tab_id().as_ref() == Some(tab_id)
            && matches!(self.center_panel, super::CenterPanel::Terminal)
        {
            return true;
        }
        let accepted = self.handle_workspace_tab_event(
            &WorkspaceTabEvent::Activate(tab_id.clone()),
            window,
            cx,
        );
        if !accepted {
            return false;
        }
        self.activate_workspace_tab(tab_id, window, cx);
        self.visible_workspace_tab_id().as_ref() == Some(tab_id)
            && matches!(self.center_panel, super::CenterPanel::Terminal)
    }

    /// 在 fixture 终端已绑定时先清掉可能继承的会话选择，避免把旧 Agent 当成验收目标。
    ///
    /// @author kongweiguang
    pub fn prepare_right_rail_acceptance_agent(
        &mut self,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.prepare_right_rail_acceptance_agent_for(AgentAcceptanceBindingScope::Ssh(host_id), cx)
    }

    /// 本地入口同样先走生产“新建对话”动作，确保焦点切到同 Tab 另一 pane 前没有旧选择。
    ///
    /// @author kongweiguang
    pub fn prepare_right_rail_acceptance_local_agent(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.prepare_right_rail_acceptance_agent_for(AgentAcceptanceBindingScope::Local, cx)
    }

    /// 两种目标共用同一面板清选动作，目标校验由 scoped context 负责。
    ///
    /// @author kongweiguang
    fn prepare_right_rail_acceptance_agent_for(
        &mut self,
        scope: AgentAcceptanceBindingScope<'_>,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(scope)?;
        Ok(panel.update(cx, |panel, cx| {
            panel.right_rail_acceptance_show_new_chat(cx)
        }))
    }

    /// 通过 Root 注入的 Preferences CAS 在本轮 Composition 配置根追加 benign Custom launcher，随后只读校验并刷新生产目录。
    /// 输出标记限制为单行可见宽度，避免 soft wrap 改写 snapshot 文本；launcher 和会话仍按完整 UUID 校验。
    ///
    /// @author kongweiguang
    pub fn configure_right_rail_acceptance_agent(
        &mut self,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) -> Result<AgentAcceptanceSetup, String> {
        self.configure_right_rail_acceptance_agent_with_fixture(
            AgentAcceptanceBindingScope::Ssh(host_id),
            AgentAcceptanceFixture::exiting(),
            cx,
        )
    }

    /// 连续性验收只追加脚本路径，命令构造仍通过同一 Preferences CAS 与 catalog 读回，不另建配置写入路径。
    ///
    /// @author kongweiguang
    pub fn configure_right_rail_acceptance_staged_agent(
        &mut self,
        host_id: &HostId,
        stage_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<AgentAcceptanceSetup, String> {
        self.configure_right_rail_acceptance_agent_with_fixture(
            AgentAcceptanceBindingScope::Ssh(host_id),
            AgentAcceptanceFixture::staged(stage_path),
            cx,
        )
    }

    /// 本地连续性入口把同一 staged CLI 安装到全新隔离配置，不使用 SSH host 或 fixture vault。
    ///
    /// @author kongweiguang
    pub fn configure_right_rail_acceptance_staged_local_agent(
        &mut self,
        stage_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<AgentAcceptanceSetup, String> {
        self.configure_right_rail_acceptance_agent_with_fixture(
            AgentAcceptanceBindingScope::Local,
            AgentAcceptanceFixture::staged(stage_path),
            cx,
        )
    }

    /// 两种夹具形态共用创建、选择、持久化和 catalog 校验；形态只影响最终 CLI 命令。
    ///
    /// @author kongweiguang
    fn configure_right_rail_acceptance_agent_with_fixture(
        &mut self,
        scope: AgentAcceptanceBindingScope<'_>,
        fixture: AgentAcceptanceFixture,
        cx: &mut Context<Self>,
    ) -> Result<AgentAcceptanceSetup, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(scope)?;
        if self.custom_agent_manager.is_some() || self.root_confirmation_owner.is_some() {
            return Err("Agent acceptance cannot replace another open modal".to_owned());
        }
        let launcher_id = Uuid::new_v4().to_string();
        let marker_nonce = Uuid::new_v4().simple().to_string();
        let output_marker = format!("rr-agent-{}", &marker_nonce[..12]);
        let setup = AgentAcceptanceSetup {
            launcher_id: launcher_id.clone(),
            output_marker: output_marker.clone(),
        };
        let preferences = self.services.preferences.clone();
        let validation = self
            .services
            .configuration_validation
            .clone()
            .ok_or_else(|| "Configuration validator is unavailable".to_owned())?;
        let profiles = self.services.profiles.clone();
        let executor = self.services.executor.clone();
        let definition_id = launcher_id.clone();
        let definition_name = format!("Right Rail Acceptance {}", &launcher_id[..8]);
        let definition_marker = output_marker.clone();
        let custom_key = format!("custom:{launcher_id}");

        cx.spawn(async move |_, cx| {
            let result = executor
                .run(move |_cancellation| async move {
                    let shell = profiles.default_local_session_target().await?;
                    let command = match &fixture {
                        AgentAcceptanceFixture::Exiting => {
                            acceptance_agent_command(&shell, &definition_marker)?
                        }
                        AgentAcceptanceFixture::Staged(staged) => {
                            acceptance_staged_agent_command(&shell, &definition_marker, staged)?
                        }
                    };
                    let definition = CustomAgentDefinition {
                        id: definition_id,
                        name: definition_name,
                        command,
                    };
                    save_and_validate_acceptance_launcher(
                        preferences,
                        validation,
                        definition,
                        custom_key,
                    )
                    .await
                })
                .await
                .map_err(|error| error.to_string());
            panel.update(cx, |panel, cx| match result {
                Ok(()) => panel.refresh_launcher_catalog_after_custom_settings_saved(cx),
                Err(message) => panel.right_rail_acceptance_record_setup_error(message, cx),
            });
        })
        .detach();
        Ok(setup)
    }

    /// 等待指定 UUID 的 custom 项同时成为 catalog 选项和当前选择，不允许 silently 回退内置 launcher。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_launcher_ready(
        &self,
        host_id: &HostId,
        launcher_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let (panel, target) = self.right_rail_acceptance_agent_context_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
        )?;
        if !panel
            .read(cx)
            .right_rail_acceptance_agent_ready_for_fixture(&target)?
        {
            return Ok(false);
        }
        panel
            .read(cx)
            .right_rail_acceptance_launcher_ready(launcher_id)
    }

    /// 本地入口不依赖 SSH fixture readiness，只在同一生产 panel 上核对 catalog、会话读回和本轮 custom key。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_launcher_ready_local(
        &self,
        launcher_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) =
            self.right_rail_acceptance_agent_context_scoped(AgentAcceptanceBindingScope::Local)?;
        let snapshot = panel.read(cx).right_rail_acceptance_snapshot();
        if snapshot.loading || !snapshot.sessions_verified {
            return Ok(false);
        }
        panel
            .read(cx)
            .right_rail_acceptance_launcher_ready(launcher_id)
    }

    /// 本地入口激活工具后等待 panel 挂载与首轮会话核验；这里只判断界面就绪，
    /// 真正的 launcher/目标错误仍由后续严格读取返回，避免把 transient 空窗伪装成成功。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_local_agent_ready(&self, cx: &gpui_kit::App) -> bool {
        let Ok((panel, _)) =
            self.right_rail_acceptance_agent_context_scoped(AgentAcceptanceBindingScope::Local)
        else {
            return false;
        };
        let snapshot = panel.read(cx).right_rail_acceptance_snapshot();
        !snapshot.loading && snapshot.sessions_verified
    }

    /// 仅当设置已加载、工作区恢复完成且没有进行中的创建时，生产“新建本地终端”入口才不会
    /// 在启动阶段被门禁直接拒绝；验收请求因此可安全地只发一次。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_local_tab_creation_ready(&self) -> bool {
        self.settings_snapshot.is_some()
            && !self.workspace_load_pending
            && !self.is_creating_session
    }

    /// 本地 Tab 创建失败诊断：返回 (安全状态串, 当前 status)。状态串含 controller/settings/
    /// workspace/creating、三个退出标志、pending creation owner 与 workspace_session_error；
    /// status 单独返回，避免从格式化串反解析。只读，不改变门禁或写入状态。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_local_tab_creation_state(&self) -> (String, Option<String>) {
        let state = format!(
            "controller_present={} settings_ready={} workspace_load_pending={} creating={} host_shutdown_requested={} pending_quit={} quit_ready={} pending_creation_owner={} workspace_session_error={:?}",
            self.workspace_controller.is_some(),
            self.settings_snapshot.is_some(),
            self.workspace_load_pending,
            self.is_creating_session,
            self.host_shutdown_requested,
            self.pending_quit_intent.is_some(),
            self.quit_ready_intent.is_some(),
            self.pending_terminal_creation_owner.is_some(),
            self.workspace_session_error
                .as_ref()
                .map(ToString::to_string),
        );
        (state, self.status.as_ref().map(ToString::to_string))
    }

    /// 返回同一生产 panel 的等待与错误状态，driver 不根据超时猜测 CLI 启动失败。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_operation_state(
        &self,
        host_id: &HostId,
        cx: &Context<Self>,
    ) -> Result<(bool, bool, bool, Option<String>), String> {
        self.right_rail_acceptance_agent_operation_state_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
            cx,
        )
    }

    /// 本地连续性入口与 SSH 入口共享同一 panel 状态读取，避免超时被误判为独立的启动失败。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_operation_state_local(
        &self,
        cx: &Context<Self>,
    ) -> Result<(bool, bool, bool, Option<String>), String> {
        self.right_rail_acceptance_agent_operation_state_scoped(
            AgentAcceptanceBindingScope::Local,
            cx,
        )
    }

    /// 只把 scope 用于目标校验，状态仍来自同一生产 AgentsPanel 快照。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_agent_operation_state_scoped(
        &self,
        scope: AgentAcceptanceBindingScope<'_>,
        cx: &Context<Self>,
    ) -> Result<(bool, bool, bool, Option<String>), String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(scope)?;
        let state = panel.read(cx).right_rail_acceptance_snapshot();
        Ok((
            state.busy,
            state.loading,
            state.sessions_verified,
            state.status,
        ))
    }

    /// 回讀選中或指定 Agent 的 production history projection，不複製或猜測標題和 launcher 身份。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_view(
        &self,
        host_id: &HostId,
        agent_session_id: Option<&AgentSessionId>,
        cx: &Context<Self>,
    ) -> Result<Option<AgentSessionView>, String> {
        self.right_rail_acceptance_agent_view_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
            agent_session_id,
            cx,
        )
    }

    /// 本地入口读取同一生产 history projection，焦点 pane 变化不能改变返回的 Agent 身份。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_view_local(
        &self,
        agent_session_id: Option<&AgentSessionId>,
        cx: &Context<Self>,
    ) -> Result<Option<AgentSessionView>, String> {
        self.right_rail_acceptance_agent_view_scoped(
            AgentAcceptanceBindingScope::Local,
            agent_session_id,
            cx,
        )
    }

    /// 目标校验只在入口发生一次，随后读取的仍是同一个 AgentsPanel 实体。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_agent_view_scoped(
        &self,
        scope: AgentAcceptanceBindingScope<'_>,
        agent_session_id: Option<&AgentSessionId>,
        cx: &Context<Self>,
    ) -> Result<Option<AgentSessionView>, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(scope)?;
        let snapshot = panel.read(cx).right_rail_acceptance_snapshot();
        Ok(match agent_session_id {
            Some(agent_session_id) => snapshot
                .sessions
                .into_iter()
                .find(|view| &view.session.agent_session_id == agent_session_id),
            None => snapshot.selected,
        })
    }

    /// 通过生产 AgentPanel 的 create action 启动本轮唯一 launcher；不调用 AgentService 绕过 UI 生命周期。
    ///
    /// @author kongweiguang
    pub fn start_right_rail_acceptance_agent(
        &mut self,
        host_id: &HostId,
        launcher_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.start_right_rail_acceptance_agent_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
            launcher_id,
            cx,
        )
    }

    /// 本地入口仍调用生产 create action；不直接向 AgentService 启动会话。
    ///
    /// @author kongweiguang
    pub fn start_right_rail_acceptance_agent_local(
        &mut self,
        launcher_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.start_right_rail_acceptance_agent_scoped(
            AgentAcceptanceBindingScope::Local,
            launcher_id,
            cx,
        )
    }

    /// 两种目标共用同一 AgentsPanel create 方法与 busy 门禁。
    ///
    /// @author kongweiguang
    fn start_right_rail_acceptance_agent_scoped(
        &mut self,
        scope: AgentAcceptanceBindingScope<'_>,
        launcher_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(scope)?;
        Ok(panel.update(cx, |panel, cx| {
            panel.right_rail_acceptance_create_agent(launcher_id, cx)
        }))
    }

    /// 对本轮运行中的精确 session 触发生产继续入口，保留其显式 target binding 写入规则。
    ///
    /// @author kongweiguang
    pub fn continue_right_rail_acceptance_agent(
        &mut self,
        host_id: &HostId,
        agent_session_id: &AgentSessionId,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
        )?;
        Ok(panel.update(cx, |panel, cx| {
            panel.right_rail_acceptance_continue_agent(agent_session_id, cx)
        }))
    }

    /// 本地历史行继续复用同一生产动作；入口不再提供内联继续，验收必须走列表语义。
    ///
    /// @author kongweiguang
    pub fn continue_right_rail_acceptance_agent_local(
        &mut self,
        agent_session_id: &AgentSessionId,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) =
            self.right_rail_acceptance_agent_context_scoped(AgentAcceptanceBindingScope::Local)?;
        Ok(panel.update(cx, |panel, cx| {
            panel.right_rail_acceptance_continue_agent(agent_session_id, cx)
        }))
    }

    /// 通过正式历史行改名流程提交标题 CAS，窗体输入和校验仍由 AgentsPanel 持有。
    ///
    /// @author kongweiguang
    pub fn rename_right_rail_acceptance_agent(
        &mut self,
        host_id: &HostId,
        agent_session_id: &AgentSessionId,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
        )?;
        Ok(panel.update(cx, |panel, cx| {
            panel.right_rail_acceptance_rename_agent(agent_session_id, title, window, cx)
        }))
    }

    /// 删除夹具会话先挂载正式确认，再由下一帧提交原 owner；保留实际归档与终端回执路径。
    ///
    /// @author kongweiguang
    pub fn archive_right_rail_acceptance_agent(
        &mut self,
        host_id: &HostId,
        agent_session_id: &AgentSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.archive_right_rail_acceptance_agent_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
            agent_session_id,
            window,
            cx,
        )
    }

    /// 本地入口在 Agent 自然退出后走同一生产归档确认与 CAS，避免隔离验收遗留运行记录。
    ///
    /// @author kongweiguang
    pub fn archive_right_rail_acceptance_agent_local(
        &mut self,
        agent_session_id: &AgentSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.archive_right_rail_acceptance_agent_scoped(
            AgentAcceptanceBindingScope::Local,
            agent_session_id,
            window,
            cx,
        )
    }

    /// 归档动作本身不感知目标来源，scope 只用于确认当前面板仍对应本轮真实终端。
    ///
    /// @author kongweiguang
    fn archive_right_rail_acceptance_agent_scoped(
        &mut self,
        scope: AgentAcceptanceBindingScope<'_>,
        agent_session_id: &AgentSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(scope)?;
        Ok(panel.update(cx, |panel, cx| {
            panel.right_rail_acceptance_archive_agent(agent_session_id, window, cx)
        }))
    }

    /// 独立回读精确会话的确认 owner，使截图发生在真正提交前而非接口受理后。
    pub fn right_rail_acceptance_agent_archive_confirmation_ready(
        &self,
        host_id: &HostId,
        agent_session_id: &AgentSessionId,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        self.right_rail_acceptance_agent_archive_confirmation_ready_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
            agent_session_id,
            cx,
        )
    }

    /// 本地入口与 SSH 入口读取同一个确认 owner，不在验收桥内另建归档状态机。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_archive_confirmation_ready_local(
        &self,
        agent_session_id: &AgentSessionId,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        self.right_rail_acceptance_agent_archive_confirmation_ready_scoped(
            AgentAcceptanceBindingScope::Local,
            agent_session_id,
            cx,
        )
    }

    /// 确认 owner 读取只依赖当前 scope 对应的真实终端，后续仍由生产 panel 决定。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_agent_archive_confirmation_ready_scoped(
        &self,
        scope: AgentAcceptanceBindingScope<'_>,
        agent_session_id: &AgentSessionId,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let (panel, _) = self.right_rail_acceptance_agent_context_scoped(scope)?;
        Ok(panel
            .read(cx)
            .right_rail_acceptance_archive_confirmation_ready(agent_session_id))
    }

    /// 只讀同一個已選 Agent PTY 的 renderer snapshot；terminal id 不符時拒絕把其它輸出算作 marker 證據。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_output(
        &self,
        host_id: &HostId,
        agent_session_id: &AgentSessionId,
        terminal_session_id: &SessionId,
    ) -> Result<Option<(SessionLifecycle, String)>, String> {
        self.right_rail_acceptance_agent_output_scoped(
            AgentAcceptanceBindingScope::Ssh(host_id),
            agent_session_id,
            terminal_session_id,
        )
    }

    /// 本地入口的 RenderSnapshot 读取与 SSH 入口相同；焦点切到同 Tab 另一 pane 不能改变被选 Agent 的终端。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_output_local(
        &self,
        agent_session_id: &AgentSessionId,
        terminal_session_id: &SessionId,
    ) -> Result<Option<(SessionLifecycle, String)>, String> {
        self.right_rail_acceptance_agent_output_scoped(
            AgentAcceptanceBindingScope::Local,
            agent_session_id,
            terminal_session_id,
        )
    }

    /// scope 仅用于确认当前可见 pane 仍是受支持的验收目标；随后严格按选中 Agent 的 terminal id 回读。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_agent_output_scoped(
        &self,
        scope: AgentAcceptanceBindingScope<'_>,
        agent_session_id: &AgentSessionId,
        terminal_session_id: &SessionId,
    ) -> Result<Option<(SessionLifecycle, String)>, String> {
        let _ = self.right_rail_acceptance_agent_context_scoped(scope)?;
        if self.selected_agent_session_id.as_ref() != Some(agent_session_id)
            || self.agent_terminal_session_id.as_ref() != Some(terminal_session_id)
        {
            return Ok(None);
        }
        let Some(snapshot) = self.agent_terminal_snapshot.as_ref() else {
            return Ok(None);
        };
        if snapshot.session_id != *terminal_session_id {
            return Err("Agent renderer snapshot belongs to a different terminal".to_owned());
        }
        let Some(lifecycle) = self.agent_terminal_lifecycle else {
            return Ok(None);
        };
        Ok(Some((lifecycle, render_snapshot_text(snapshot))))
    }

    /// 只读当前活动 Tab 挂载的 Agent 面板身份与选择；连续性与换挂校验不能从服务快照猜面板状态。
    /// 读取和普通选中都复用生产实体，不触发 reload、重绑或终端 watcher。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_panel_state(
        &self,
        cx: &gpui_kit::App,
    ) -> AgentContinuityPanelState {
        let snapshot = self
            .agents_panel
            .as_ref()
            .map(|panel| panel.read(cx).right_rail_acceptance_snapshot());
        let selected = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.selected.clone());
        AgentContinuityPanelState {
            panel_tab_id: self.agent_panel_tab_id.clone(),
            panel_entity_id: self
                .agents_panel
                .as_ref()
                .map(|panel| panel.entity_id().as_u64()),
            selected_agent_session_id: selected
                .as_ref()
                .map(|view| view.session.agent_session_id.clone()),
            selected_terminal_session_id: selected
                .as_ref()
                .and_then(|view| view.terminal.as_ref())
                .map(|terminal| terminal.session_id.clone()),
            terminal_lifecycle: self.agent_terminal_lifecycle,
            target_generation: selected
                .as_ref()
                .and_then(|view| view.session.target.as_ref())
                .map(|target| target.generation),
            target_terminal_session_id: selected
                .as_ref()
                .and_then(|view| view.session.target.as_ref())
                .and_then(|target| target.terminal_session_id.clone()),
            history_mode: snapshot
                .as_ref()
                .map_or(AgentHistoryMode::CurrentTarget, |snapshot| {
                    snapshot.history_mode
                }),
            session_count: snapshot.map_or(0, |snapshot| snapshot.sessions.len()),
        }
    }

    /// 通过生产新建终端入口创建第二个本地 Tab，让连续性场景覆盖真实 controller 注册、激活与面板换挂。
    /// 仍受既有停机、工作区加载和并发创建门禁约束，不在验收桥内直接改布局快照。
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_local_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_terminal_for_target(TerminalTargetRequest::ProfileDefaultLocal, window, cx);
    }

    /// 返回指定终端 Tab 内已就绪的真实 pane/session 对，供验收识别分屏后实际创建的新会话。
    /// 直接读 controller 快照与 runtime，不构造占位 pane，也不修改布局。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_terminal_panes(&self, tab_id: &TabId) -> Vec<(PaneId, SessionId)> {
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Vec::new();
        };
        let snapshot = controller.snapshot();
        let Some(WorkspaceSessionTab::Terminal { layout, .. }) =
            snapshot.terminal_tabs.iter().find(|tab| tab.id() == tab_id)
        else {
            return Vec::new();
        };
        layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane_id| {
                let pane = self.workspace_runtime.pane(&pane_id)?;
                if pane.tab_id != *tab_id || pane.phase != super::RuntimePanePhase::Ready {
                    return None;
                }
                Some((pane_id, pane.session_id.clone()?))
            })
            .collect()
    }

    /// 验收专用的回收存在性判据：`Ok(())` 只在运行态 pane、session 映射与已提交布局都不再持有
    /// 目标时返回；否则用可读原因说明还残留哪一侧。不能复用 `right_rail_acceptance_terminal_panes`，
    /// 因为它同时过滤布局外与 Ready 之外的 pane，会把 Closing/CloseFailed 以及“布局已移除但运行态
    /// 仍持有”的 pane 隐藏成看起来已回收。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_pane_removed(
        &self,
        tab_id: &TabId,
        pane_id: &PaneId,
        session_id: &SessionId,
    ) -> Result<(), String> {
        let presence = continuity_pane_presence(
            self.workspace_controller
                .as_ref()
                .map(|controller| controller.snapshot()),
            &self.workspace_runtime,
            tab_id,
            pane_id,
            session_id,
        );
        if presence.fully_removed() {
            return Ok(());
        }
        Err(format!(
            "运行态 pane 仍存在：{}，session 仍映射到该 pane：{}，布局仍引用该 pane：{}",
            presence.pane_present, presence.session_bound, presence.layout_present
        ))
    }

    /// 仅返回当前活动 Tab 的 Ready 焦点 pane 与其真实 SessionId；焦点切换必须由生产事件完成后再读。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_focused_terminal(&self) -> Option<(TabId, PaneId, SessionId)> {
        let snapshot = self.workspace_controller.as_ref()?.snapshot();
        let tab_id = snapshot.active_tab_id.clone()?;
        let pane_id = snapshot.focused_pane_id.clone()?;
        let pane = self.workspace_runtime.pane(&pane_id)?;
        if pane.tab_id != tab_id || pane.phase != super::RuntimePanePhase::Ready {
            return None;
        }
        Some((tab_id, pane_id, pane.session_id.clone()?))
    }

    /// 通过 Root 正常布局事件创建同一 Tab 的本地分屏；新 pane 的会话仍由 controller 的
    /// CreatePane 资源和既有 Profile 解析产生，验收不直接写布局或伪造 session。
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_local_split(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_workspace_render_event(
            super::workspace_render::WorkspaceRenderEvent::Pane {
                tab_id,
                event: WorkspacePaneEvent::Split {
                    pane_id,
                    direction: SplitDirection::Horizontal,
                    before: false,
                },
            },
            window,
            cx,
        );
    }

    /// 使用 Root 的生产 pane Activate 路径切换同 Tab 焦点，保留焦点、输入目标与面板刷新语义。
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_focus_local_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_workspace_pane(tab_id, pane_id, window, cx);
    }

    /// 通过 Root 的生产 pane 关闭路径回收验收分屏：复用 `handle_workspace_render_event`
    /// 对 `WorkspacePaneEvent::Close` 的完整处理（末 pane 折叠为 Tab 关闭、deferred split
    /// 关闭意图、controller `begin_close_pane`、资源 executor `close_until`、成功后
    /// `remove_closed_session`）。只有这条路径会同时移除 workspace_runtime pane 与候选布局；
    /// 服务级 `close_until` 只关闭 PTY，pane 会带着已死会话留在 Ready 状态。会话身份参数
    /// 仅用于 fail-closed 核对：pane 没有绑定到调用方记录的同一会话时直接拒绝，防止关闭
    /// 错误目标。验收环境保证无模态与传输在途，确认门禁会按无阻塞直通处理。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_close_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        expected_session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_matches = self.workspace_runtime.pane(&pane_id).is_some_and(|pane| {
            pane.tab_id == tab_id
                && pane.phase == super::workspace_runtime::RuntimePanePhase::Ready
                && pane.session_id.as_ref() == Some(&expected_session_id)
        });
        if !session_matches {
            self.status = Some(crate::locale::text(
                "验收关闭目标不是该 Tab 中绑定预期会话的就绪 pane，关闭未执行。",
                "The acceptance close target is not a ready pane bound to the expected session in this tab; close was not run.",
            ));
            cx.notify();
            return;
        }
        self.handle_workspace_render_event(
            super::workspace_render::WorkspaceRenderEvent::Pane {
                tab_id,
                event: crate::features::workspace_panes::WorkspacePaneEvent::Close(pane_id),
            },
            window,
            cx,
        );
    }

    /// 暴露同一共享服务实例给验收驱动，隐藏期间按 session ID 读输出和写输入，不新增生产入口。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_services(&self) -> AppServices {
        self.services.clone()
    }

    /// 原生 activity 验收读取主窗口 observer 的激活/失活次数；只读，不触发窗口动作。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_window_activation_events(&self) -> (usize, usize) {
        (
            self.acceptance_activation_events,
            self.acceptance_deactivation_events,
        )
    }

    /// 在辅助窗口切换前清零，避免把此前显示/激活主窗口的事件算进本轮 activity。
    ///
    /// @author kongweiguang
    pub fn reset_right_rail_acceptance_window_activation_events(&mut self) {
        self.acceptance_activation_events = 0;
        self.acceptance_deactivation_events = 0;
    }

    /// 原生首帧取证：读取可见工具落到恢复占位的次数；普通生产不读取。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_unavailable_fallback_render_count(&self) -> usize {
        self.plugin_tools.unavailable_fallback_render_count()
    }

    /// 在 activity 前清零，确保计数只覆盖本轮失活/激活过渡。
    ///
    /// @author kongweiguang
    pub fn reset_right_rail_acceptance_unavailable_fallback_render_count(&mut self) {
        self.plugin_tools.reset_unavailable_fallback_renders();
    }

    /// 每次 Agent acceptance bridge 调用都重核活动工具、目标 pane、panel 与 candidate，不复用过期目标。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_agent_context_scoped(
        &self,
        scope: AgentAcceptanceBindingScope<'_>,
    ) -> Result<
        (
            gpui_kit::Entity<crate::features::agents::AgentsPanel>,
            kerminal_domain::agents::AgentTargetBinding,
        ),
        String,
    > {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::AgentLauncher) {
            return Err("Agent 工具不是当前验收面板".to_owned());
        }
        let fixture = self
            .right_rail_acceptance_acceptance_binding(scope)?
            .ok_or_else(|| match scope {
                AgentAcceptanceBindingScope::Ssh(_) => {
                    "Agent 验收没有活动的 fixture SSH pane".to_owned()
                }
                AgentAcceptanceBindingScope::Local => {
                    "Agent 验收没有活动的本地终端 pane".to_owned()
                }
            })?;
        let candidate = self
            .current_agent_target_candidate()
            .ok_or_else(|| "Agent 验收没有完整的活动终端候选".to_owned())?;
        let target = candidate.binding;
        let matches_target = match scope {
            AgentAcceptanceBindingScope::Ssh(host_id) => {
                target.target_kind.as_deref() == Some("ssh")
                    && target.target_ref.as_deref() == Some(host_id.as_str())
                    && target.terminal_session_id.as_ref() == Some(&fixture.session_id)
            }
            AgentAcceptanceBindingScope::Local => {
                target.target_kind.as_deref() == Some("local")
                    && target.target_ref.is_none()
                    && target.terminal_session_id.as_ref() == Some(&fixture.session_id)
            }
        };
        if !matches_target {
            return Err(match scope {
                AgentAcceptanceBindingScope::Ssh(_) => {
                    "Agent 候选与活动 fixture SSH pane 不匹配".to_owned()
                }
                AgentAcceptanceBindingScope::Local => {
                    "Agent 候选与活动本地终端 pane 不匹配".to_owned()
                }
            });
        }
        let panel = self
            .agents_panel
            .clone()
            .ok_or_else(|| "Agent panel 不可用".to_owned())?;
        Ok((panel, target))
    }
}

/// 只针对应用投影做 CAS 保存、只读 Settings 校验和读回；Composition 的配置根由 acceptance harness 隔离。
///
/// @author kongweiguang
async fn save_and_validate_acceptance_launcher(
    preferences: PreferencesService,
    validation: ConfigurationValidationService,
    definition: CustomAgentDefinition,
    custom_key: String,
) -> AppResult<()> {
    let mut current = preferences.load_settings().await?;
    current
        .settings
        .agent_launcher
        .custom_agents
        .push(definition.clone());
    current.settings.agent_launcher.selected_agent_key = custom_key.clone();
    preferences
        .save_settings(current.settings, current.revision)
        .await?;

    let report = validation
        .validate(ConfigurationValidationScope::Settings)
        .await?;
    if !report.valid {
        return Err(AppError::invalid(
            "Agent acceptance Settings validation failed",
        ));
    }

    let readback = preferences.load_settings().await?;
    let saved_definition = readback
        .settings
        .agent_launcher
        .custom_agents
        .iter()
        .find(|candidate| candidate.id == definition.id);
    if readback.settings.agent_launcher.selected_agent_key != custom_key
        || saved_definition != Some(&definition)
    {
        return Err(AppError::conflict(
            "Agent acceptance Settings readback did not match the isolated launcher",
        ));
    }
    Ok(())
}

/// 按 Agent resolver 实际使用的本地默认 shell 生成单条无网络命令，避免依赖错误 shell 语法或覆盖用户 profile。
///
/// @author kongweiguang
fn acceptance_agent_command(shell: &LocalSessionTarget, marker: &str) -> AppResult<String> {
    ensure_acceptance_command_mode_is_clear(shell)?;
    let basename = acceptance_shell_basename(shell)?;
    match basename.as_str() {
        "powershell.exe" | "powershell" | "pwsh.exe" | "pwsh" => {
            Ok(format!("Write-Output '{marker}'; Start-Sleep -Seconds 8"))
        }
        "cmd.exe" | "cmd" => Ok(format!("echo {marker} & timeout /t 8 >NUL")),
        _ => Err(AppError::unsupported(
            "Agent acceptance requires PowerShell or cmd",
        )),
    }
}

/// 连续性夹具作为外层 pwsh 7 的 `-Command` 脚本文本执行，不生成脚本文件、不改执行策略。
/// 触发路径必须在本轮独立目录中；PowerShell 单引号字面量可原样容纳空格、`$`、`%` 和反引号，
/// 单引号按既有 `quote_shell_argument` 规则转成两个单引号。cmd 外层无法安全区分 `%` 展开，
/// Windows PowerShell 5.1 的语义也未验证，因此连续性夹具明确拒绝这两类 profile。
///
/// 触发文件只作为无内容存在性信号，脚本不读取正文；路径每轮唯一，避免 Test-Path 与写入竞态。
/// 模板末尾不能带换行：设置 TOML 的多行字面量会在闭合分隔符前裁掉尾换行，验收读回按原字符串
/// 精确比较，保留尾换行会把持久化成功误报成 launcher 不匹配。
///
/// @author kongweiguang
const AGENT_CONTINUITY_COMMAND_TEMPLATE: &str = r#"$ErrorActionPreference = 'Stop'
# ConPTY 下 -Command 脚本的 stdin 可能保留行输入模式，KeyAvailable 在 Enter 前看不到草稿；
# 显式关闭行输入/回显，保留其它控制位，才能读取未提交的草稿字符。
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class KerminalConsoleMode {
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr GetStdHandle(int handle);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool GetConsoleMode(IntPtr handle, out uint mode);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool SetConsoleMode(IntPtr handle, uint mode);
}
"@
$stdinHandle = [KerminalConsoleMode]::GetStdHandle(-10)
$consoleMode = 0
if ([KerminalConsoleMode]::GetConsoleMode($stdinHandle, [ref]$consoleMode)) {
    [void][KerminalConsoleMode]::SetConsoleMode($stdinHandle, $consoleMode -band (-bnot 0x6))
}
[Console]::Out.Write("__MARKER__-psversion:$($PSVersionTable.PSVersion.Major)`n")
[Console]::Out.Write("__MARKER__-ready`n__MARKER__-prompt> ")
[Console]::Out.Flush()
$buffer = New-Object System.Text.StringBuilder
$StageFile = __STAGE__
function Write-Prompt {
  [Console]::Out.Write("`n__MARKER__-prompt> $($buffer.ToString())")
  [Console]::Out.Flush()
}
while ($true) {
  if (Test-Path -LiteralPath $StageFile) {
    Remove-Item -LiteralPath $StageFile -Force
    [Console]::Out.Write("`n__MARKER__-stage")
    Write-Prompt
  }
  if ([Console]::KeyAvailable) {
    $key = [Console]::ReadKey($true)
    if ($key.Key -eq [ConsoleKey]::Enter) {
      $line = $buffer.ToString()
      [void]$buffer.Clear()
      [Console]::Out.Write("`n__MARKER__-line:$line")
      Write-Prompt
      if ($line -ceq 'exit') { break }
    } elseif ($key.Key -eq [ConsoleKey]::Backspace) {
      if ($buffer.Length -gt 0) {
        [void]$buffer.Remove($buffer.Length - 1, 1)
        [Console]::Out.Write("`b `b")
        [Console]::Out.Flush()
      }
    } elseif (-not [char]::IsControl($key.KeyChar)) {
      [void]$buffer.Append($key.KeyChar)
      [Console]::Out.Write($key.KeyChar)
      [Console]::Out.Flush()
    }
  }
  Start-Sleep -Milliseconds 50
}
[Console]::Out.Write("__MARKER__-exiting`n")
[Console]::Out.Flush()"#;

/// 只允许本轮生成的十六进制 marker 进入脚本文本，避免历史或外部字符串拼出额外命令片段。
///
/// @author kongweiguang
fn ensure_acceptance_marker(marker: &str) -> AppResult<()> {
    let suffix = marker
        .strip_prefix("rr-agent-")
        .ok_or_else(|| AppError::invalid("Agent acceptance marker has an unexpected prefix"))?;
    if suffix.len() != 12
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(AppError::invalid(
            "Agent acceptance marker must be twelve lowercase hex digits",
        ));
    }
    Ok(())
}

/// 只允许 `terminal_acceptance::powershell_arguments_are_interactive` 同款交互参数，
/// 拒绝执行策略、启动脚本和其它未知参数；验收还要求显式 `-NoProfile`，不能依赖生产
/// launcher 事后补参数，否则保存的 profile 可能在未来调用路径中先加载用户脚本。
///
/// @author kongweiguang
fn continuity_powershell_arguments_are_interactive(args: &[std::ffi::OsString]) -> bool {
    let mut has_no_profile = false;
    let recognized = args.iter().all(|argument| {
        argument.to_str().is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            if value == "-noprofile" {
                has_no_profile = true;
            }
            matches!(
                value.as_str(),
                "-nologo" | "-noexit" | "-noprofile" | "-noprofileloadtime"
            )
        })
    });
    recognized && has_no_profile
}

/// 连续性夹具依赖 pwsh 7 的内联脚本语义，并且只接受纯交互参数且显式关闭 profile 的外层配置。
/// `-NoProfileLoadTime` 不能替代 `-NoProfile`，因为它并不保证所有 profile 路径都被跳过；
/// Windows PowerShell 5.1、cmd、`-ExecutionPolicy Bypass` 或用户启动脚本都不满足隔离边界，直接拒绝。
///
/// @author kongweiguang
fn ensure_continuity_pwsh_profile(shell: &LocalSessionTarget) -> AppResult<()> {
    let basename = acceptance_shell_basename(shell)?;
    if !matches!(basename.as_str(), "pwsh.exe" | "pwsh") {
        return Err(AppError::unsupported(
            "Agent continuity acceptance requires pwsh 7 as the default profile shell",
        ));
    }
    if !continuity_powershell_arguments_are_interactive(&shell.args) {
        return Err(AppError::unsupported(
            "Agent continuity acceptance requires explicit -NoProfile and rejects non-interactive or policy-changing pwsh profile arguments",
        ));
    }
    Ok(())
}

/// PowerShell 7 词法分析器把 U+2018–U+201B 当作单引号终止符，即使它们位于 ASCII 单引号字面量中
/// 也会提前闭合；既有 `quote_shell_argument` 只转义 ASCII `'`，所以这里必须拒绝这些字符，
/// 而不是扩大共享生产 helper 的语义或尝试自行编码。
///
/// @author kongweiguang
fn is_powershell_single_quote_terminator(character: char) -> bool {
    matches!(character, '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}')
}

/// 复用既有 PowerShell 单引号引用规则，并拒绝控制字符、无效 UTF-8 与会被 PowerShell 当作
/// 单引号终止符的智能引号，确保整段路径始终是同一个字面量而不是可执行文本。
///
/// @author kongweiguang
fn continuity_stage_literal(stage_path: &std::path::Path) -> AppResult<String> {
    let stage = stage_path
        .to_str()
        .filter(|value| !value.chars().any(char::is_control))
        .filter(|value| !value.chars().any(is_powershell_single_quote_terminator))
        .ok_or_else(|| AppError::invalid("Agent acceptance stage path is not safe text"))?;
    quote_shell_argument(stage, RemoteShellDialect::PowerShell)
        .ok_or_else(|| AppError::invalid("Agent acceptance stage path cannot be quoted safely"))
}

/// 组装固定模板、marker 和已安全引用的触发路径；模板不含用户输入，替换顺序也固定，
/// 这样验收命令始终是可读的单条 pwsh 脚本文本，不经过脚本文件或编码层。
///
/// @author kongweiguang
fn acceptance_staged_agent_command(
    shell: &LocalSessionTarget,
    marker: &str,
    fixture: &AgentAcceptanceStagedFixture,
) -> AppResult<String> {
    ensure_acceptance_command_mode_is_clear(shell)?;
    ensure_continuity_pwsh_profile(shell)?;
    ensure_acceptance_marker(marker)?;
    let stage = continuity_stage_literal(&fixture.stage_path)?;
    // 先替换固定格式 marker，再替换可能包含任意字面量的 stage，避免路径内容被二次替换。
    Ok(AGENT_CONTINUITY_COMMAND_TEMPLATE
        .replace("__MARKER__", marker)
        .replace("__STAGE__", &stage))
}

/// 默认 shell 已预置命令模式时，验收命令会被当作 file 参数或提前退出，必须保存 launcher 前失败。
///
/// @author kongweiguang
fn ensure_acceptance_command_mode_is_clear(shell: &LocalSessionTarget) -> AppResult<()> {
    let basename = acceptance_shell_basename(shell)?;
    match basename.as_str() {
        "powershell.exe" | "powershell" | "pwsh.exe" | "pwsh" => {
            if shell.args.iter().any(|argument| {
                argument.to_str().is_some_and(|argument| {
                    matches!(
                        argument.to_ascii_lowercase().as_str(),
                        "-command" | "-c" | "-file" | "-encodedcommand" | "-e"
                    )
                })
            }) {
                return Err(AppError::invalid(
                    "Default PowerShell profile already specifies a command mode",
                ));
            }
        }
        "cmd.exe" | "cmd" => {
            if shell.args.iter().any(|argument| {
                argument.to_str().is_some_and(|argument| {
                    matches!(argument.to_ascii_lowercase().as_str(), "/c" | "/k")
                })
            }) {
                return Err(AppError::invalid(
                    "Default cmd profile already specifies a command mode",
                ));
            }
        }
        _ => {
            return Err(AppError::unsupported(
                "Agent acceptance requires PowerShell or cmd",
            ));
        }
    }
    Ok(())
}

/// shell 可执行文件必须能以 UTF-8 文本表示；非法平台路径不能退回猜测的 PowerShell 方言。
///
/// @author kongweiguang
fn acceptance_shell_basename(shell: &LocalSessionTarget) -> AppResult<String> {
    let program = shell
        .program
        .to_str()
        .ok_or_else(|| AppError::invalid("Default local shell path is not valid text"))?;
    Ok(program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase())
}

/// 將 renderer 的可見 run 依 viewport 行順序拼成純文字，供唯一 marker 做精確回讀。
///
/// @author kongweiguang
fn render_snapshot_text(snapshot: &RenderSnapshot) -> String {
    snapshot
        .rows
        .iter()
        .map(|row| {
            row.runs
                .iter()
                .map(|run| run.text.as_ref())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "../../tests/unit/root_agent_continuity.rs"]
mod tests;
