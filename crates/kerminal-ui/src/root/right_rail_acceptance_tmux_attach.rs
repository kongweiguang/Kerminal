//! Isolated right-rail tmux attach/detach acceptance bridge for the active fixture pane.
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::{
    preferences::settings::ToolRailToolId, sessions::UiSessionBinding,
    workspace::WorkspaceTargetRef,
};
use kerminal_domain::{identity::HostId, sessions::SessionOwner};

use super::RootView;

impl RootView {
    /// Captures the exact fixture UI session binding that this run must return to after tmux exits.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_fixture_binding(
        &self,
        host_id: &HostId,
    ) -> Result<Option<UiSessionBinding>, String> {
        self.validate_right_rail_acceptance_tmux_attach_context(host_id, None)?;
        self.right_rail_acceptance_fixture_binding(host_id)
    }

    /// Looks up only this run's fixture session through the fixed tmux Operations snapshot.
    /// The outer Option means the service snapshot is not ready; the inner Option means name absence.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_session_id_for_name(
        &self,
        host_id: &HostId,
        session_name: &str,
        binding: &UiSessionBinding,
        cx: &Context<Self>,
    ) -> Result<Option<Option<String>>, String> {
        self.validate_right_rail_acceptance_tmux_attach_context(host_id, Some(binding))?;
        self.right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .map_or(Ok(None), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_tmux_session_id_for_name(host_id, session_name, binding)
            })
    }

    /// Runs Operations' production Attach request using the fixture session and current terminal binding.
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_tmux_attach(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        session_name: &str,
        binding: &UiSessionBinding,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_tmux_attach_context(host_id, Some(binding))?;
        if self.root_confirmation_owner.is_some() {
            return Err("another Root confirmation is active".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_request_tmux_attach(
                host_id,
                session_id,
                session_name,
                binding,
                view_cx,
            )
        })
    }

    /// Reads the exact fixture session's production client count, never a neighboring tmux target.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_session_clients(
        &self,
        host_id: &HostId,
        session_id: &str,
        session_name: &str,
        binding: &UiSessionBinding,
        cx: &Context<Self>,
    ) -> Result<Option<(bool, u32)>, String> {
        self.validate_right_rail_acceptance_tmux_attach_context(host_id, Some(binding))?;
        self.right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .map_or(Ok(None), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_tmux_session_clients(
                        host_id,
                        session_id,
                        session_name,
                        binding,
                    )
            })
    }

    /// Starts the next production tmux read only after the prior read settled, for bounded live polling.
    ///
    /// @author kongweiguang
    pub fn refresh_right_rail_acceptance_tmux_snapshot(
        &mut self,
        host_id: &HostId,
        binding: &UiSessionBinding,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.validate_right_rail_acceptance_tmux_attach_context(host_id, Some(binding))?;
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_refresh_tmux_snapshot(host_id, binding, view_cx)
        })
    }

    /// Invokes the production header exit request only after this run's session has a live client.
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_tmux_header_exit(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        session_name: &str,
        binding: &UiSessionBinding,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_tmux_attach_context(host_id, Some(binding))?;
        if self.root_confirmation_owner.is_some() {
            return Err("another Root confirmation is active".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_request_tmux_header_exit(
                host_id,
                session_id,
                session_name,
                binding,
                view_cx,
            )
        })
    }

    /// Reports whether the production FIFO handler has finished its request for the same fixture owner.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_detach_idle(
        &self,
        host_id: &HostId,
        binding: &UiSessionBinding,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_tmux_attach_context(host_id, Some(binding))?;
        self.right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .map_or(Ok(false), |operations| {
                operations
                    .read(cx)
                    .right_rail_acceptance_tmux_detach_idle(host_id, binding)
            })
    }

    /// Restores the tab and pane that owned the original fixture binding, rejecting retired or rebound sessions.
    ///
    /// @author kongweiguang
    pub fn restore_right_rail_acceptance_tmux_fixture_tab(
        &mut self,
        host_id: &HostId,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.root_confirmation_owner.is_some() {
            return Err(
                "Root confirmation is still active; fixture focus was not changed".to_owned(),
            );
        }
        let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
            return Err("The saved fixture binding is not owned by a UI pane".to_owned());
        };
        if self.workspace_runtime.session_for_pane(pane_id) != Some(&binding.session_id)
            || self.terminal_view_bindings.get(&binding.session_id) != Some(binding)
        {
            return Err("The saved fixture session binding is retired or rebound".to_owned());
        }
        let pane = self
            .workspace_runtime
            .pane(pane_id)
            .ok_or_else(|| "The saved fixture pane no longer exists".to_owned())?;
        if pane.tab_id != *tab_id
            || pane.phase != super::super::RuntimePanePhase::Ready
            || !matches!(pane.target.as_ref(), Some(super::super::SessionTarget::Ssh(target)) if target.host_id == *host_id)
            || !matches!(pane.persisted.target.as_ref(), Some(WorkspaceTargetRef::Ssh { host_id: persisted }) if persisted == host_id)
        {
            return Err("The saved fixture pane no longer targets the same SSH host".to_owned());
        }
        let tab_contains_fixture = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller.snapshot().terminal_tabs.iter().any(|tab| {
                    tab.id() == tab_id
                        && tab
                            .terminal_layout()
                            .is_some_and(|layout| layout.contains_pane(pane_id))
                })
            });
        if !tab_contains_fixture || self.terminal_snapshot_for_binding(binding).is_none() {
            return Err("The saved fixture terminal tab or live session is unavailable".to_owned());
        }

        self.activate_workspace_tab(tab_id, window, cx);
        self.focus_workspace_pane(tab_id.clone(), pane_id.clone(), window, cx);
        Ok(())
    }

    /// Rechecks the active tmux tool, Root modal state, host and full UI session generation on every bridge call.
    ///
    /// @author kongweiguang
    fn validate_right_rail_acceptance_tmux_attach_context(
        &self,
        host_id: &HostId,
        expected_binding: Option<&UiSessionBinding>,
    ) -> Result<(), String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Tmux) {
            return Err("tmux tool is not the active acceptance panel".to_owned());
        }
        let binding = self
            .right_rail_acceptance_fixture_binding(host_id)?
            .ok_or_else(|| "the active pane is not the ready fixture SSH session".to_owned())?;
        if expected_binding.is_some_and(|expected| expected != &binding) {
            return Err("the active fixture pane owner or generation changed".to_owned());
        }
        Ok(())
    }
}
