//! Hidden-window acceptance hooks that reuse Root's production theme and host flows.
//!
//! @author kongweiguang

#[path = "right_rail_acceptance_containers.rs"]
mod containers;
#[path = "right_rail_acceptance_shell.rs"]
mod shell;
#[path = "right_rail_acceptance_tmux_attach.rs"]
mod tmux_attach;
pub use shell::RightRailAcceptanceShellSnapshot;

use gpui_kit::{Context, Entity, Window, component::WindowExt as _};
use kerminal_application::preferences::{ThemeMode, settings::ToolRailToolId};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    commands::CommandHistorySource,
    connections::RemoteProtocol,
    identity::HostId,
    sessions::{SessionOwner, SessionTarget},
};

use crate::features::operations::OperationsSection;

use super::RootView;

impl RootView {
    /// Reports whether the normal settings load has finished before acceptance overrides its theme.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_settings_ready(&self) -> bool {
        self.settings_snapshot.is_some()
    }

    /// Requests the fixture through the same protocol-checked host action used by the connections UI.
    /// A missing row returns `false` so the harness can wait for the asynchronous host snapshot.
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_fixture_connection(
        &mut self,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) -> Result<bool, &'static str> {
        if self.connection_prompt_modal_open {
            return Err("fixture SSH host-key confirmation is pending");
        }
        let Some(panel) = self.connections_panel.clone() else {
            return Err("host connection panel is unavailable");
        };
        Ok(panel.update(cx, |panel, cx| {
            panel.request_connect_host_for_protocol(host_id, RemoteProtocol::Ssh, cx)
        }))
    }

    /// Accepts readiness only when the active pane targets this SSH host and has a live UI snapshot.
    /// Unexpected host-key confirmation fails closed so acceptance never trusts an unknown key.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_fixture_ready(
        &self,
        host_id: &HostId,
    ) -> Result<bool, &'static str> {
        if self.connection_prompt_modal_open {
            return Err("fixture SSH host-key confirmation is pending; refusing automatic trust");
        }
        let Some(session_id) = self.active_session_id() else {
            return Ok(false);
        };
        let Some(pane_id) = self.workspace_runtime.pane_for_session(&session_id) else {
            return Ok(false);
        };
        let Some(pane) = self.workspace_runtime.pane(pane_id) else {
            return Ok(false);
        };
        if !matches!(
            pane.target.as_ref(),
            Some(SessionTarget::Ssh(target)) if target.host_id.as_str() == host_id.as_str()
        ) {
            return Ok(false);
        }
        let Some(binding) = self.terminal_view_bindings.get(&session_id) else {
            return Ok(false);
        };
        Ok(self.terminal_snapshot_for_binding(binding).is_some())
    }

    /// Waits for the selected production panel's own successful read, always tied to the active SSH pane.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_panel_ready(
        &self,
        tool: ToolRailToolId,
        host_id: &HostId,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(tool) {
            return Ok(false);
        }
        let Some(binding) = self.right_rail_acceptance_fixture_binding(host_id)? else {
            return Ok(false);
        };

        match tool {
            ToolRailToolId::AgentLauncher => {
                let Some(candidate) = self.current_agent_target_candidate() else {
                    return Ok(false);
                };
                let target = candidate.binding;
                if target.target_kind.as_deref() != Some("ssh")
                    || target.target_ref.as_deref() != Some(host_id.as_str())
                    || target.terminal_session_id.as_ref() != Some(&binding.session_id)
                {
                    return Err("Agent target does not match the active fixture pane".to_owned());
                }
                self.agents_panel.as_ref().map_or(Ok(false), |panel| {
                    panel
                        .read(cx)
                        .right_rail_acceptance_agent_ready_for_fixture(&target)
                })
            }
            ToolRailToolId::Sftp => {
                Err("SFTP acceptance readiness requires the fixture directory".to_owned())
            }
            ToolRailToolId::Snippets => self.commands_view.as_ref().map_or(Ok(false), |view| {
                Ok(view
                    .read(cx)
                    .right_rail_acceptance_snippet_directory_ready(&binding, cx))
            }),
            ToolRailToolId::Logs => self.logs_view.as_ref().map_or(Ok(false), |view| {
                Ok(view.read(cx).right_rail_acceptance_history_ready(&binding))
            }),
            ToolRailToolId::Containers
            | ToolRailToolId::Tmux
            | ToolRailToolId::Ports
            | ToolRailToolId::System => {
                let section = match tool {
                    ToolRailToolId::Containers => OperationsSection::Containers,
                    ToolRailToolId::Tmux => OperationsSection::Tmux,
                    ToolRailToolId::Ports => OperationsSection::PortForwards,
                    ToolRailToolId::System => OperationsSection::ServerInfo,
                    _ => unreachable!("operations section was selected above"),
                };
                self.right_rail_acceptance_operations_view(tool)
                    .map_or(Ok(false), |view| {
                        view.read(cx).right_rail_acceptance_ready(section, host_id)
                    })
            }
            ToolRailToolId::Context => {
                Err("Current Context is outside right-rail acceptance scope".to_owned())
            }
        }
    }

    /// 将 SFTP 验收目标限制为当前活动工具和可信 fixture SSH pane，再委托 FilesPane 走普通目录导航。
    ///
    /// @author kongweiguang
    pub fn navigate_right_rail_acceptance_sftp_directory(
        &mut self,
        host_id: &HostId,
        directory: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Sftp) {
            return Err("SFTP 工具不是当前验收面板".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
        {
            return Err("SFTP 验收没有活动的 fixture SSH pane".to_owned());
        }
        let panel = self
            .sftp_panel
            .clone()
            .ok_or_else(|| "SFTP 文件面板不可用".to_owned())?;
        panel.update(cx, |panel, panel_cx| {
            panel.right_rail_acceptance_open_directory(host_id, directory, panel_cx)
        })
    }

    /// 截图就绪必须同时匹配活动 fixture 会话、SFTP 面板 endpoint 和该目录的已提交 listing。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_directory_ready(
        &self,
        host_id: &HostId,
        directory: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Sftp)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(false);
        }
        self.sftp_panel.as_ref().map_or(Ok(false), |panel| {
            panel
                .read(cx)
                .right_rail_acceptance_ready(host_id, directory)
        })
    }

    /// 页面切换固定到已就绪的验收 SSH pane，避免测试入口绕过 Root 的实时目标绑定。
    ///
    /// @author kongweiguang
    pub fn select_right_rail_acceptance_system_page(
        &mut self,
        page: &str,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::System) {
            return Err("System 工具不是当前验收面板".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
        {
            return Err("System 验收没有活动的 fixture SSH pane".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::System)
            .ok_or_else(|| "System 运维视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_select_server_page(page, host_id, view_cx)
        })
    }

    /// System 页面完成条件同时验证活动 SSH pane、固定右栏目标和同目标服务器采样。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_system_page_ready(
        &self,
        page: &str,
        host_id: &HostId,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::System)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(false);
        }
        self.right_rail_acceptance_operations_view(ToolRailToolId::System)
            .map_or(Ok(false), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_server_page_ready(page, host_id)
            })
    }

    /// 只返回当前 SSH host 快照中名称精确匹配的运行容器 ID，避免验收误选其它容器。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_container_target_id(
        &self,
        host_id: &HostId,
        fixture_container_name: &str,
        cx: &Context<Self>,
    ) -> Result<Option<String>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(None);
        }
        self.right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .map_or(Ok(None), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_container_target_id(host_id, fixture_container_name)
            })
    }

    /// 经当前活动 fixture SSH pane 委托生产容器面板返回宿主列表。
    ///
    /// @author kongweiguang
    pub fn return_right_rail_acceptance_container_list(
        &mut self,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers) {
            return Err("容器工具不是当前验收面板".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
        {
            return Err("容器验收没有活动的 fixture SSH pane".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器运维视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_return_to_container_list(host_id, view_cx)
        })
    }

    /// 打开详情或监控时绑定原容器 ID；Root 的读回执仍由现有 OperationsEvent 流程处理。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_container_page(
        &mut self,
        host_id: &HostId,
        container_id: &str,
        page: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers) {
            return Err("容器工具不是当前验收面板".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
        {
            return Err("容器验收没有活动的 fixture SSH pane".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器运维视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_open_container_page(host_id, container_id, page, view_cx)
        })
    }

    /// 只在 inspector 仍展示已读取的原容器 target 时允许生成对应状态截图。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_container_page_ready(
        &self,
        host_id: &HostId,
        container_id: &str,
        page: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(false);
        }
        self.right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .map_or(Ok(false), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_container_page_ready(host_id, container_id, page)
            })
    }

    /// 只返回当前 tmux capability 下属于 fixture SSH host 的真实 session ID。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_session_id(
        &self,
        host_id: &HostId,
        cx: &Context<Self>,
    ) -> Result<Option<String>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Tmux)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(None);
        }
        self.right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .map_or(Ok(None), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_tmux_session_id(host_id)
            })
    }

    /// 展开操作前后都核对 session 仍属于当前 SSH host 的 tmux 快照。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_session_ready(
        &self,
        host_id: &HostId,
        session_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Tmux)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(false);
        }
        self.right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .map_or(Ok(false), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_tmux_session_ready(host_id, session_id)
            })
    }

    /// 通过当前 host 的 tmux 运维视图切换展开态，保持其生产折叠语义和重绘路径。
    ///
    /// @author kongweiguang
    pub fn set_right_rail_acceptance_tmux_session_expanded(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Tmux) {
            return Err("tmux 工具不是当前验收面板".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
        {
            return Err("tmux 验收没有活动的 fixture SSH pane".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux 运维视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_set_tmux_session_expanded(
                host_id, session_id, expanded, view_cx,
            )
        })
    }

    /// Resolves the actual focused pane binding and refuses stale, non-SSH, or host-mismatched targets.
    ///
    /// @author kongweiguang
    pub(super) fn right_rail_acceptance_fixture_binding(
        &self,
        host_id: &HostId,
    ) -> Result<Option<UiSessionBinding>, String> {
        if self.connection_prompt_modal_open {
            return Err("fixture SSH host-key confirmation is pending".to_owned());
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
        let Some(SessionTarget::Ssh(target)) = pane.target.as_ref() else {
            return Err("active fixture pane is not an SSH terminal".to_owned());
        };
        if target.host_id != *host_id {
            return Err("active fixture pane is bound to a different SSH host".to_owned());
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

    /// Applies one acceptance theme through Root's normal Kit, tool-panel, and terminal palette paths.
    /// The temporary mode is kept in memory and never saved to the user's configuration.
    ///
    /// @author kongweiguang
    pub fn set_right_rail_acceptance_theme(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        self.theme_mode = mode;
        crate::theme::apply_theme_mode(mode, self.window_appearance, cx);
        self.refresh_tool_rail_panel_theme(cx);
        self.queue_terminal_palette_update(cx);
        cx.notify();
    }

    /// Opens the same non-destructive toolbar customizer invoked by the rail's production context action.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_customizer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.open_tool_rail_customizer(window, cx);
        self.tool_rail_customizer
            .as_ref()
            .is_some_and(|dialog| dialog.read(cx).is_open())
    }

    /// Closes through the same retained-owner transition as the Cancel button; popping Kit's stack alone leaves the owner open.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_customizer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(customizer) = self.tool_rail_customizer.clone() else {
            return false;
        };
        if !customizer.read(cx).is_open() || customizer.read(cx).is_saving() {
            return false;
        }
        customizer.update(cx, |customizer, cx| customizer.mark_closed(cx));
        window.close_dialog(cx);
        true
    }

    /// Opens the production Agent manager from the isolated acceptance configuration.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_agent_manager(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.open_custom_agent_manager(window, cx);
        self.custom_agent_manager.is_some()
    }

    /// Requests the manager's normal guarded close path; Root releases the entity after its Close event.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_agent_manager(&mut self, cx: &mut Context<Self>) -> bool {
        if self.custom_agent_manager.is_none() {
            return false;
        }
        self.request_close_custom_agent_manager(cx);
        true
    }

    /// Confirms the manager was actually removed after its normal Root event has been processed.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_agent_manager_closed(&self) -> bool {
        self.custom_agent_manager.is_none()
    }

    /// Opens the production snippet manager on its shared CommandsView overlay.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_snippet_manager(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(commands) = self.commands_view.clone() else {
            return false;
        };
        commands.update(cx, |view, cx| view.open_snippet_library_manager(cx));
        commands.read(cx).snippet_overlay_open() && !commands.read(cx).snippet_editor_open()
    }

    /// Requests the production snippet manager close and preserves its cancellation and busy-state rules.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_snippet_manager(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(commands) = self.commands_view.clone() else {
            return false;
        };
        if !commands.read(cx).snippet_overlay_open() || commands.read(cx).snippet_editor_open() {
            return false;
        }
        commands.update(cx, |view, cx| view.close_snippet_library_manager(cx));
        true
    }

    /// Waiters also require Root's shared snippet modal flag to settle after Commands emits visibility.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_manager_closed(&self, cx: &Context<Self>) -> bool {
        !self.snippet_editor_modal_open
            && self
                .commands_view
                .as_ref()
                .is_some_and(|commands| !commands.read(cx).snippet_overlay_open())
    }

    /// Opens the blank editor through Commands' production new-snippet intent without saving it.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_snippet_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(commands) = self.commands_view.clone() else {
            return false;
        };
        commands.update(cx, |view, cx| view.new_snippet(window, cx));
        commands.read(cx).snippet_editor_open()
    }

    /// Requests an untouched editor close through the normal overlay path, preserving draft and save gates.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_snippet_editor(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(commands) = self.commands_view.clone() else {
            return false;
        };
        if !commands.read(cx).snippet_editor_open() {
            return false;
        }
        commands.update(cx, |view, cx| view.close_snippet_overlay(cx));
        true
    }

    /// Confirms both Commands and Root released the modal after their normal visibility event.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_snippet_editor_closed(&self, cx: &Context<Self>) -> bool {
        !self.snippet_editor_modal_open
            && self
                .commands_view
                .as_ref()
                .is_some_and(|commands| !commands.read(cx).snippet_overlay_open())
    }

    /// Returns only the Operations entity hosted by the requested rail tool, not the center workbench view.
    ///
    /// @author kongweiguang
    pub(super) fn right_rail_acceptance_operations_view(
        &self,
        tool: ToolRailToolId,
    ) -> Option<gpui_kit::Entity<crate::features::operations::OperationsView>> {
        self.tool_rail_operations_views.get(&tool).cloned()
    }

    /// Opens the production tmux creation confirmation against the already mounted tmux rail target.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_tmux_create(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Tmux) {
            return false;
        }
        let Some(operations) = self.right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
        else {
            return false;
        };
        if operations.read(cx).root_confirmation_active() {
            return false;
        }
        operations.update(cx, |view, cx| view.request_tmux_create_dialog(window, cx));
        operations.read(cx).root_confirmation_active()
    }

    /// Requests cancellation through Operations' shared confirmation state machine.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_tmux_create(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(operations) = self.right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
        else {
            return false;
        };
        if !operations.read(cx).root_confirmation_active() {
            return false;
        }
        operations.update(cx, |view, cx| view.cancel_root_confirmation(cx));
        true
    }

    /// Confirms Operations and Root both released the asynchronous confirmation owner.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_create_closed(&self, cx: &Context<Self>) -> bool {
        self.root_confirmation_owner.is_none()
            && self
                .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
                .is_some_and(|operations| !operations.read(cx).root_confirmation_active())
    }

    /// Opens the production add-tunnel form using the Ports tool's current frozen host target.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_forward_add(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Ports) {
            return false;
        }
        let Some(operations) = self.right_rail_acceptance_operations_view(ToolRailToolId::Ports)
        else {
            return false;
        };
        if operations.read(cx).root_confirmation_active() {
            return false;
        }
        operations.update(cx, |view, cx| view.prepare_forward_add(window, cx));
        operations.read(cx).root_confirmation_active()
    }

    /// Requests cancellation of the production add-tunnel form, retaining the normal pending-write guard.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_forward_add(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(operations) = self.right_rail_acceptance_operations_view(ToolRailToolId::Ports)
        else {
            return false;
        };
        if !operations.read(cx).root_confirmation_active() {
            return false;
        }
        operations.update(cx, |view, cx| view.cancel_forward_form(cx));
        true
    }

    /// Confirms the tunnel form and Root confirmation owner both settled after the cancellation event.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_add_closed(&self, cx: &Context<Self>) -> bool {
        self.root_confirmation_owner.is_none()
            && self
                .right_rail_acceptance_operations_view(ToolRailToolId::Ports)
                .is_some_and(|operations| !operations.read(cx).root_confirmation_active())
    }

    /// Opens the production create-directory dialog on the SFTP pane bound to the active fixture session.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_file_create_directory(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Sftp) {
            return false;
        }
        let Some(files) = self.sftp_panel.clone() else {
            return false;
        };
        if files.read(cx).pending_global_dialog() {
            return false;
        }
        files.update(cx, |pane, cx| pane.prepare_create_directory(window, cx));
        files.read(cx).pending_global_dialog()
    }

    /// 原生验收必须验证真实 Files 关闭按钮持有焦点，不能把 Root 陷阱句柄当作 main 的初始焦点。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_dialog_close_focused(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> bool {
        self.sftp_panel.as_ref().is_some_and(|files| {
            files
                .read(cx)
                .right_rail_acceptance_dialog_close_focused(window)
        })
    }

    /// Requests cancellation of the unsent create-directory draft through FilesPane's standard dialog path.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_file_create_directory(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(files) = self.sftp_panel.clone() else {
            return false;
        };
        if !files.read(cx).pending_global_dialog() {
            return false;
        }
        files.update(cx, |pane, cx| pane.cancel_global_dialog(cx));
        true
    }

    /// Confirms FilesPane and the Root modal owner both released the directory dialog.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_create_directory_closed(&self, cx: &Context<Self>) -> bool {
        self.root_confirmation_owner.is_none()
            && self
                .sftp_panel
                .as_ref()
                .is_some_and(|files| !files.read(cx).pending_global_dialog())
    }

    /// Closes the harness's single active rail panel through its normal toggle path before a baseline.
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_panels(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tool) = self.right_rail_acceptance_active_tool() {
            self.toggle_tool_rail_panel(tool, window, cx);
        }
    }

    /// 通过当前生产 Logs Entity 写入带唯一前缀的历史种子；同一 run 重试复用回执而不重复写入。
    ///
    /// @author kongweiguang
    pub fn begin_right_rail_acceptance_logs(
        &mut self,
        host_id: &HostId,
        run_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.update(cx, |view, cx| {
            view.right_rail_acceptance_seed_history(run_id, &binding, window, cx)
        })
    }

    /// 同时等待种子写入回执和生产 Logs 对唯一前缀的查询完成。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_logs_ready(
        &self,
        host_id: &HostId,
        run_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.read(cx)
            .right_rail_acceptance_seed_ready(run_id, &binding)
    }

    /// 向验收驱动器回读本次种子前缀及当前 Logs 分页。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_logs_snapshot(
        &self,
        host_id: &HostId,
        run_id: &str,
        cx: &Context<Self>,
    ) -> Result<crate::features::commands::RightRailAcceptanceLogsSnapshot, String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.read(cx)
            .right_rail_acceptance_snapshot(run_id, &binding)
    }

    /// 保留本次命令前缀，通过生产查询输入路径修改搜索条件。
    ///
    /// @author kongweiguang
    pub fn set_right_rail_acceptance_logs_query(
        &mut self,
        host_id: &HostId,
        run_id: &str,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.update(cx, |view, cx| {
            view.right_rail_acceptance_set_query(run_id, &binding, query, window, cx)
        })
    }

    /// 通过 Logs 的生产来源筛选路径修改来源并复位页码。
    ///
    /// @author kongweiguang
    pub fn set_right_rail_acceptance_logs_source(
        &mut self,
        host_id: &HostId,
        run_id: &str,
        source: Option<CommandHistorySource>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.update(cx, |view, cx| {
            view.right_rail_acceptance_set_source(run_id, &binding, source, cx)
        })
    }

    /// 通过生产分页边界逻辑切换本次种子结果页。
    ///
    /// @author kongweiguang
    pub fn set_right_rail_acceptance_logs_page(
        &mut self,
        host_id: &HostId,
        run_id: &str,
        page: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.update(cx, |view, cx| {
            view.right_rail_acceptance_set_page(run_id, &binding, page, cx)
        })
    }

    /// 通过 Logs 生产删除路径删除当前可见的种子记录。
    ///
    /// @author kongweiguang
    pub fn delete_right_rail_acceptance_logs_entry(
        &mut self,
        host_id: &HostId,
        run_id: &str,
        entry_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.update(cx, |view, cx| {
            view.right_rail_acceptance_delete_entry(run_id, &binding, entry_id, cx)
        })
    }

    /// 校验唯一前缀后，通过 Logs 生产清空路径清理隔离 fixture pane。
    ///
    /// @author kongweiguang
    pub fn clear_right_rail_acceptance_logs(
        &mut self,
        host_id: &HostId,
        run_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (binding, view) = self.right_rail_acceptance_logs_context(host_id)?;
        view.update(cx, |view, cx| {
            view.right_rail_acceptance_clear_history(run_id, &binding, cx)
        })
    }

    /// 重新校验活动 Logs 工具、fixture host 和终端快照 binding 后解析实体。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_logs_context(
        &self,
        host_id: &HostId,
    ) -> Result<
        (
            UiSessionBinding,
            Entity<crate::features::commands::LogsView>,
        ),
        String,
    > {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Logs) {
            return Err("Logs 工具不是当前验收面板".to_owned());
        }
        let binding = self
            .right_rail_acceptance_fixture_binding(host_id)?
            .ok_or_else(|| "日志验收没有活动的 fixture SSH pane".to_owned())?;
        let view = self
            .logs_view
            .clone()
            .ok_or_else(|| "Logs 工具视图不可用".to_owned())?;
        Ok((binding, view))
    }
}
