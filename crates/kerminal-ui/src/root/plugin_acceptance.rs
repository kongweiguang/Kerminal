//! 仅验收构建可用的插件观测入口；动作仍通过生产宿主和工作区状态机。
//!
//! @author kongweiguang

use super::*;

impl RootView {
    /// 观察当前真实 Commands 实体和通过工具宿主确认的视图，便于绘制后核对唯一挂载。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_commands_snapshot(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> serde_json::Value {
        let commands = self.commands_view.clone();
        let entity_id = commands.as_ref().map(|view| view.entity_id());
        let current = self.tab_tool_contents_are_current();
        let editor_open = super::snippet_editor::is_open(self);
        let context = self.plugin_tool_context(cx);
        let window_active = window.is_window_active() || self.right_rail_acceptance_active_override;
        let rail_view_id = self
            .plugin_tools
            .view_for_mount(ToolRailToolId::Snippets, &context, window_active)
            .map(|view| view.entity_id());
        let rail_mounted = current
            && !editor_open
            && entity_id.is_some()
            && entity_id == rail_view_id
            && self.snippets_mounted_in_tool_rail(window, cx);
        let (compact, focused) = commands.as_ref().map_or((None, false), |view| {
            let (compact, focused) = view.read(cx).plugin_acceptance_layout_and_focus(window, cx);
            (Some(compact), focused)
        });
        serde_json::json!({
            "entityId": entity_id.map(|id| format!("{id:?}")),
            "railViewId": rail_view_id.map(|id| format!("{id:?}")),
            "centerRoute": format!("{:?}", self.center_panel),
            "centerMounted": current && !editor_open && entity_id.is_some()
                && self.center_panel == CenterPanel::Commands,
            "railMounted": rail_mounted,
            "editorMounted": current && editor_open && entity_id.is_some(),
            "compactLayout": compact,
            "searchFocused": focused,
            "viewportCompact": self.tool_rail_rendered_compact.0,
            "activeTabId": self.workspace_controller.as_ref()
                .and_then(|controller| controller.snapshot().active_tab_id.as_ref())
                .map(TabId::as_str),
        })
    }

    /// 普通打开走生产页面入口，验收可核对已有宿主被定位而不是被替换。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_open_commands(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        self.open_plugin_page("commands", window, cx)
    }

    /// 验收调用标题栏相同的宿主动作，不能用独立路径制造预期布局。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_move_commands_to_rail(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        self.move_snippets_host(
            crate::features::commands::SnippetHostMoveRequested::ToolRail,
            window,
            cx,
        )
    }

    /// 验收复用标题栏的移动动作，实际宿主和焦点仍由生产路径决定。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_move_commands_to_center(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        self.move_snippets_host(
            crate::features::commands::SnippetHostMoveRequested::Center,
            window,
            cx,
        )
    }

    /// 只读实际实例、保存状态和活动身份，测试不能用伪造快照代替真实打开。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_snapshot(&self, cx: &Context<Self>) -> serde_json::Value {
        use crate::plugins::content::ContentScope;
        let tabs = self.workspace_controller.as_ref().map(|controller| {
            controller.snapshot().terminal_tabs.iter().filter_map(|tab| {
                let WorkspaceSessionTab::Extension { id, content_id, state_version, state, .. } = tab else { return None; };
                let scope = ContentScope::Tab(id.clone());
                let live_state = self.content_host.snapshot(&scope, cx).ok();
                Some(serde_json::json!({
                    "tabId": id.as_str(), "contentId": content_id,
                    "savedVersion": state_version, "savedState": state,
                    "phase": self.content_host.phase(&scope).map(|phase| format!("{phase:?}")),
                    "liveVersion": live_state.as_ref().map(|state| state.version),
                    "liveState": live_state.map(|state| state.value),
                    "viewId": self.content_host.view(&scope).map(|view| format!("{:?}", view.entity_id())),
                }))
            }).collect::<Vec<_>>()
        }).unwrap_or_default();
        serde_json::json!({
            "tabs": tabs,
            "activeTabId": self.workspace_controller.as_ref().and_then(|controller| controller.snapshot().active_tab_id.as_ref()).map(TabId::as_str),
            "instanceCount": self.content_host.scopes().len(),
            "centralContent": self.center_panel == CenterPanel::Contributed,
            "status": self.status.as_ref().map(|status| status.as_str()),
            "workspaceReady": self.workspace_controller.is_some(),
            "connectionsModalOpen": self.connections_modal_open,
            "pendingHostOpen": self.pending_plugin_host_open.is_some(),
            "selectedPluginHostId": self.selected_plugin_host_id.as_ref().map(HostId::as_str),
            "navigationGeneration": self.surface_navigation_generation,
        })
    }

    /// 激活与关闭都经过同一生产动作；验收不能跳过取消、草稿或资源回收门禁。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_tab_action(
        &mut self,
        tab_id: TabId,
        close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let event = if close {
            WorkspaceTabEvent::Close(tab_id)
        } else {
            WorkspaceTabEvent::Activate(tab_id)
        };
        if self.handle_workspace_tab_event(&event, window, cx) {
            self.activate_current_workspace_surface_after_close(window, cx);
        }
    }

    /// 通过已有生产入口提取并保存当前实例状态，用于重启往返验收。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_save(&mut self, cx: &mut Context<Self>) {
        self.persist_workspace_snapshot(cx);
    }

    /// 主机 fixture 也必须进入真实提供方异步流程，不能直接创建期望的测试 Tab。
    ///
    /// @author kongweiguang
    pub fn plugin_acceptance_open_host(
        &mut self,
        host_id: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_plugin_host(host_id, window, cx);
    }

    /// 原生验收选择插件主机类型后打开同一连接编辑器模态；表单实例仍由生产面板创建。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn plugin_host_form_acceptance_activate(
        &mut self,
        type_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let panel = self
            .connections_panel
            .clone()
            .ok_or_else(|| kerminal_application::AppError::not_found("连接面板不可用"))?;
        panel.update(cx, |panel, cx| {
            panel.acceptance_activate_plugin_form(type_id, window, cx)
        });
        if !self.prepare_connection_editor_modal(window, cx) {
            return Err(kerminal_application::AppError::conflict("连接编辑器未打开"));
        }
        Ok(())
    }

    /// 只读回活动插件表单的身份、阶段、控件与面板文案，供验收断言真实 UI 状态。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn plugin_host_form_acceptance_snapshot(&self, cx: &gpui_kit::App) -> serde_json::Value {
        let panel = self
            .connections_panel
            .as_ref()
            .and_then(|panel| panel.read(cx).acceptance_plugin_form_snapshot(cx));
        serde_json::json!({
            "modalOpen": self.connections_modal_open,
            "panel": panel,
            "rootStatus": self.status.as_ref().map(ToString::to_string),
        })
    }

    /// 验收写入活动表单的真实输入控件；不支持验收的表单由 trait 默认实现拒绝。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn plugin_host_form_acceptance_set_field(
        &mut self,
        path: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let panel = self
            .connections_panel
            .clone()
            .ok_or_else(|| kerminal_application::AppError::not_found("连接面板不可用"))?;
        panel.update(cx, |panel, cx| {
            panel.acceptance_plugin_form_set_field(path, value, window, cx)
        })
    }

    /// 测试走生产按钮同一入口，不绕过取消、控冻结与回执状态。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn plugin_host_form_acceptance_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let panel = self
            .connections_panel
            .clone()
            .ok_or_else(|| kerminal_application::AppError::not_found("连接面板不可用"))?;
        panel.update(cx, |panel, cx| {
            panel.acceptance_plugin_form_test(window, cx)
        });
        Ok(())
    }

    /// 保存走生产保存按钮同一入口，Vault 准备与 CAS 仍由正式服务完成。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn plugin_host_form_acceptance_save(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let panel = self
            .connections_panel
            .clone()
            .ok_or_else(|| kerminal_application::AppError::not_found("连接面板不可用"))?;
        panel.update(cx, |panel, cx| {
            panel.acceptance_plugin_form_save(window, cx)
        });
        Ok(())
    }
}
