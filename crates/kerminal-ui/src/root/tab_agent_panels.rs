//! 按标签保活右侧 Agent 面板；历史归属不限制 Agent 的全局工具权限。
//!
//! @author kongweiguang

use gpui_kit::{AppContext as _, Context, Entity, Subscription, Window};
use kerminal_domain::agents::AgentSessionScope;

use super::RootView;
use crate::features::agents::AgentsPanel;

pub(super) struct RetainedAgentPanel {
    panel: Entity<AgentsPanel>,
    subscription: Option<Subscription>,
}

impl RootView {
    /// 后台标签的异步回执只更新原面板，不得更换当前 CLI 或夺取当前窗口焦点。
    ///
    /// @author kongweiguang
    pub(super) fn subscribe_agent_panel(
        panel: &Entity<AgentsPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(panel, window, |root, source, event, window, cx| {
            if root.agents_panel.as_ref() != Some(source) {
                return;
            }
            match event {
                crate::features::agents::AgentPanelEvent::NewTerminalFocusStarted {
                    request_generation,
                } => {
                    root.pending_tool_rail_placement_focus = None;
                    root.begin_agent_creation_focus_intent(*request_generation, window, cx);
                }
                crate::features::agents::AgentPanelEvent::NewTerminalFocusReady {
                    request_generation,
                    agent_session_id,
                    terminal_session_id,
                } => {
                    root.complete_agent_creation_focus_intent(
                        *request_generation,
                        agent_session_id.clone(),
                        terminal_session_id.clone(),
                        window,
                        cx,
                    );
                }
                crate::features::agents::AgentPanelEvent::NewTerminalFocusCancelled {
                    request_generation,
                } => {
                    root.cancel_agent_creation_focus_intent(*request_generation);
                }
                crate::features::agents::AgentPanelEvent::Selected {
                    agent_session_id,
                    terminal_session_id,
                } => {
                    if root.host_shutdown_requested {
                        return;
                    }
                    root.retain_agent_creation_focus_for_selection(
                        agent_session_id.as_ref(),
                        terminal_session_id.as_ref(),
                    );
                    root.select_agent_terminal(
                        agent_session_id.clone(),
                        terminal_session_id.clone(),
                        window,
                        cx,
                    );
                    super::command_blocks::agent::select_pending_send_target(
                        root,
                        agent_session_id.as_ref(),
                        terminal_session_id.as_ref(),
                        cx,
                    );
                    if let Some(panel) = root.mcp_runtime_panel.as_ref() {
                        panel.update(cx, |panel, cx| panel.refresh(cx));
                    }
                }
                crate::features::agents::AgentPanelEvent::PendingSendConfirmed {
                    agent_session_id,
                    request_id,
                    preview_id,
                } => {
                    super::command_blocks::agent::confirm_pending_send(
                        root,
                        agent_session_id.clone(),
                        request_id,
                        preview_id,
                        cx,
                    );
                }
                crate::features::agents::AgentPanelEvent::PendingSendCancelled { request_id } => {
                    super::command_blocks::agent::cancel_pending_send(root, request_id, cx);
                }
                crate::features::agents::AgentPanelEvent::OpenCustomAgentManager => {
                    root.open_custom_agent_manager(window, cx);
                }
                crate::features::agents::AgentPanelEvent::OpenAgentLauncherSettings => {
                    if root.host_shutdown_requested {
                        return;
                    }
                    root.open_preferences_modal(window, cx);
                    root.preferences_panel.update(cx, |panel, cx| {
                        panel.show_agent_launcher_settings(window, cx);
                    });
                }
                crate::features::agents::AgentPanelEvent::TargetBindingChanged => {}
            }
        })
    }

    /// 只在活动标签身份变化时换挂既有实体，保留草稿与选择且不重建、重启或重绑 CLI。
    /// 关闭标签当轮就释放其视图缓存和订阅；Agent 进程仍由 AgentService 的显式生命周期管理。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_agent_panel_for_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agents) = self.services.agents.clone() else {
            return;
        };
        let tab_id = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone());
        let live_tabs = self
            .workspace_controller
            .as_ref()
            .map(|controller| {
                controller
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .map(|tab| tab.id().clone())
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        self.retained_agent_panels
            .retain(|key, _| key.as_ref().is_none_or(|id| live_tabs.contains(id)));
        if self.agent_panel_tab_id == tab_id {
            return;
        }
        let outgoing_tab_id = self.agent_panel_tab_id.take();
        let outgoing_subscription = self._agent_event_subscription.take();
        if let Some(panel) = self.agents_panel.take()
            && outgoing_tab_id
                .as_ref()
                .is_none_or(|id| live_tabs.contains(id))
        {
            self.retained_agent_panels.insert(
                outgoing_tab_id,
                RetainedAgentPanel {
                    panel,
                    subscription: outgoing_subscription,
                },
            );
        }
        let retained = self
            .retained_agent_panels
            .remove(&tab_id)
            .unwrap_or_else(|| {
                let executor = self.services.executor.clone();
                let scope = tab_id.clone().map_or(AgentSessionScope::Global, |tab_id| {
                    AgentSessionScope::Tab { tab_id }
                });
                let panel = cx.new(|cx| AgentsPanel::new(agents, executor, scope, window, cx));
                let subscription = Some(Self::subscribe_agent_panel(&panel, window, cx));
                RetainedAgentPanel {
                    panel,
                    subscription,
                }
            });
        self.agent_panel_tab_id = tab_id;
        self.agents_panel = Some(retained.panel.clone());
        self._agent_event_subscription = retained.subscription;
        let candidate = self.current_agent_target_candidate();
        let target_summary = self.current_agent_target_summary();
        let theme_mode = self.theme_mode;
        let palette = self.current_terminal_palette();
        let (agent_id, session_id) = retained.panel.update(cx, |panel, cx| {
            panel.set_theme_mode(theme_mode, cx);
            panel.set_terminal_palette(palette, cx);
            panel.set_target_candidate(candidate.clone(), cx);
            panel.set_target_summary(Some(target_summary), cx);
            (
                panel.selected_agent_session_id(),
                panel.selected_terminal_session_id(),
            )
        });
        self.agent_target_candidate = super::CachedAgentTargetCandidate(candidate);
        self.pending_agent_focus_intent = None;
        let dock_collapsed = self.right_dock_collapsed;
        self.select_agent_terminal(agent_id, session_id, window, cx);
        self.right_dock_collapsed = dock_collapsed;
        super::command_blocks::agent::active_agent_panel_changed(self, cx);
    }
}
