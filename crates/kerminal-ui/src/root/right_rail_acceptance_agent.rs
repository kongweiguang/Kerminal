//! Agent 右栏生产流程的隔离配置与状态回读桥。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::{
    AppError, AppResult, PreferencesService,
    agents::AgentSessionView,
    preferences::{
        ConfigurationValidationScope, ConfigurationValidationService, CustomAgentDefinition,
        settings::ToolRailToolId,
    },
};
use kerminal_domain::{
    identity::{AgentSessionId, HostId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionLifecycle, SessionOwner},
};
use kerminal_terminal::snapshot::RenderSnapshot;
use uuid::Uuid;

use crate::features::workspace_tabs::WorkspaceTabEvent;

use super::RootView;

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

impl RootView {
    /// Captures only when the validated fixture pane is also owned by Root's currently visible Terminal tab.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_fixture_tab_id(&self, host_id: &HostId) -> Result<TabId, String> {
        let binding = self
            .right_rail_acceptance_fixture_binding(host_id)?
            .ok_or_else(|| "Agent acceptance has no active fixture SSH pane".to_owned())?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
                    let command = acceptance_agent_command(&shell, &definition_marker)?;
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
        let (panel, target) = self.right_rail_acceptance_agent_context(host_id)?;
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

    /// 返回同一生产 panel 的等待与错误状态，driver 不根据超时猜测 CLI 启动失败。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_operation_state(
        &self,
        host_id: &HostId,
        cx: &Context<Self>,
    ) -> Result<(bool, bool, bool, Option<String>), String> {
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let (panel, _) = self.right_rail_acceptance_agent_context(host_id)?;
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
        let _ = self.right_rail_acceptance_agent_context(host_id)?;
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

    /// 每次 Agent acceptance bridge 调用都重核活动工具、SSH host、panel 与 candidate，不复用过期目标。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_agent_context(
        &self,
        host_id: &HostId,
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
            .right_rail_acceptance_fixture_binding(host_id)?
            .ok_or_else(|| "Agent 验收没有活动的 fixture SSH pane".to_owned())?;
        let candidate = self
            .current_agent_target_candidate()
            .ok_or_else(|| "Agent 验收没有完整的活动终端候选".to_owned())?;
        let target = candidate.binding;
        if target.target_kind.as_deref() != Some("ssh")
            || target.target_ref.as_deref() != Some(host_id.as_str())
            || target.terminal_session_id.as_ref() != Some(&fixture.session_id)
        {
            return Err("Agent 候选与活动 fixture SSH pane 不匹配".to_owned());
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
    let program = shell
        .program
        .to_str()
        .ok_or_else(|| AppError::invalid("Default local shell path is not valid text"))?;
    let basename = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
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
            Ok(format!("Write-Output '{marker}'; Start-Sleep -Seconds 8"))
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
            Ok(format!("echo {marker} & timeout /t 8 >NUL"))
        }
        _ => Err(AppError::unsupported(
            "Agent acceptance requires PowerShell or cmd",
        )),
    }
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
