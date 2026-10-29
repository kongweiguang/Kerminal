//! Acceptance-only access to the production rail projection and tab navigation.
//!
//! @author kongweiguang

use crate::features::workspace_tabs::WorkspaceTabEvent;
use gpui_kit::{App, Context, Window};
use kerminal_application::preferences::{ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId};
use kerminal_domain::identity::TabId;

use super::RootView;

/// Read-only state returned to the desktop acceptance driver without exposing Root's internal owners.
///
/// @author kongweiguang
pub struct RightRailAcceptanceShellSnapshot {
    slots: [Option<ToolRailToolId>; 4],
    active_tab: Option<TabId>,
    tab_ids: Vec<TabId>,
    settings: ToolRailSettings,
    compact_panels: bool,
    compact_active_tool: Option<ToolRailToolId>,
    compact_tool_drawer_open: bool,
}

impl RightRailAcceptanceShellSnapshot {
    /// Returns the normalized attached, left, bottom, and center panel occupants.
    ///
    /// @author kongweiguang
    pub fn slots(&self) -> [Option<ToolRailToolId>; 4] {
        self.slots
    }

    /// Returns the active workspace tab without transferring ownership from the snapshot.
    ///
    /// @author kongweiguang
    pub fn active_tab(&self) -> Option<&TabId> {
        self.active_tab.as_ref()
    }

    /// Returns the workspace tab identities in their current order.
    ///
    /// @author kongweiguang
    pub fn tab_ids(&self) -> &[TabId] {
        &self.tab_ids
    }

    /// Returns the settings snapshot used to normalize the panel projection.
    ///
    /// @author kongweiguang
    pub fn settings(&self) -> &ToolRailSettings {
        &self.settings
    }

    /// Returns the rendered responsive breakpoint instead of inferring it from saved preferences.
    ///
    /// @author kongweiguang
    pub fn compact_panels(&self) -> bool {
        self.compact_panels
    }

    /// Returns the active tool selected by the compact projection for the current Tab.
    ///
    /// @author kongweiguang
    pub fn compact_active_tool(&self) -> Option<ToolRailToolId> {
        self.compact_active_tool
    }

    /// Returns whether the compact tool drawer is mounted open for the current Tab.
    ///
    /// @author kongweiguang
    pub fn compact_tool_drawer_open(&self) -> bool {
        self.compact_tool_drawer_open
    }
}

impl RootView {
    /// Reads the real rail slots, active workspace tab, tab identities, and current settings.
    /// The projection is used so the acceptance harness observes the same normalized state as render.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_shell_snapshot(&mut self) -> RightRailAcceptanceShellSnapshot {
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        let workspace = self
            .workspace_controller
            .as_ref()
            .map(|controller| controller.snapshot());
        let active_tab = workspace
            .as_ref()
            .and_then(|snapshot| snapshot.active_tab_id.clone());
        let tab_ids = workspace
            .map(|snapshot| {
                snapshot
                    .terminal_tabs
                    .iter()
                    .map(|tab| tab.id().clone())
                    .collect()
            })
            .unwrap_or_default();
        RightRailAcceptanceShellSnapshot {
            slots: [panels.attached, panels.left, panels.bottom, panels.center],
            active_tab,
            tab_ids,
            settings,
            compact_panels: self.tool_rail_rendered_compact.0,
            compact_active_tool: self.tool_rail_rendered_compact.1,
            compact_tool_drawer_open: self.compact_tool_drawer_open,
        }
    }

    /// Reports a configured placement from Root's current settings snapshot.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_shell_placement(
        &self,
        tool: ToolRailToolId,
    ) -> ToolRailPanelPlacement {
        self.current_tool_rail_settings()
            .effective_panel_placement(tool)
    }

    /// Drives one normal rail toggle so open, close, replacement, and slot projection share production logic.
    ///
    /// @author kongweiguang
    pub fn toggle_right_rail_acceptance_shell_tool(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_tool_rail_panel(tool, window, cx);
    }

    /// Mirrors the production tab subscription: the controller must accept activation before Root projects it.
    /// Calling only the projection leaves WorkspaceFile tabs inactive and would test an impossible click path.
    ///
    /// @author kongweiguang
    pub fn activate_right_rail_acceptance_shell_tab(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let event = WorkspaceTabEvent::Activate(tab_id.clone());
        if self.handle_workspace_tab_event(&event, window, cx) {
            self.activate_workspace_tab(tab_id, window, cx);
        }
    }

    /// Reads the retained customizer owner so the driver waits for the real dialog close or save result.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_shell_customizer_open(&self, cx: &App) -> bool {
        self.tool_rail_customizer
            .as_ref()
            .is_some_and(|customizer| customizer.read(cx).is_open())
    }
}
