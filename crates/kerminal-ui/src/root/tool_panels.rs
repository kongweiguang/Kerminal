//! ToolRail 面板的唯一宿主装配与工作区冲突投影。
//!
//! @author kongweiguang

use std::path::PathBuf;

use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, KeyDownEvent, Window, div,
    prelude::*, px,
};
use kerminal_application::{
    platform::external_launch::ExternalTargetLease,
    preferences::{ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId},
    sessions::UiSessionBinding,
};
use kerminal_domain::{files::FileEndpoint, identity::HostId};

use crate::{
    features::operations::{OperationsEvent, OperationsSection, OperationsView},
    theme::ThemeTokens,
};

use crate::features::operations::{
    ActiveOperationsPaneTarget, ToolRailOperationTarget, resolve_tool_rail_operation_target,
};

use super::{
    AppServices, CenterPanel, RootView, SessionOwner, SessionTarget, WorkspaceTargetRef,
    keybinding_focus,
    keybindings::KeybindingFocusScope,
    logical_input_modal_blocked,
    workbench::{
        ToolRailContextAction, ToolRailPanelSlots, WorkbenchToolPanelRenderers,
        WorkbenchToolPanels, attached_dock_default_width, focused_workspace_pane,
    },
    workspace_runtime::RuntimePanePhase,
};

/// 明确打开与快捷保存共享一次居中焦点来源；保存仍串行复用 Preferences 的 Ready/CAS 门禁。
pub(super) struct ToolRailPlacementFocusIntent {
    tool: ToolRailToolId,
    navigation_generation: u64,
    expected_focus: FocusHandle,
}

impl RootView {
    /// 活动 terminal pane 优先；只有当前中心没有 terminal context 时才回退到明确选中的 SSH 主机。
    ///
    /// @author kongweiguang
    fn active_operations_pane_target(&self) -> ActiveOperationsPaneTarget {
        if !center_preserves_terminal_file_context(self.center_panel) {
            return ActiveOperationsPaneTarget::Absent;
        }
        let Some(controller) = self.workspace_controller.as_ref() else {
            return ActiveOperationsPaneTarget::Absent;
        };
        let snapshot = controller.snapshot();
        let Some(tab_id) = snapshot.active_tab_id.as_ref() else {
            return ActiveOperationsPaneTarget::Absent;
        };
        let Some(tab) = snapshot.terminal_tabs.iter().find(|tab| tab.id() == tab_id) else {
            return ActiveOperationsPaneTarget::Unavailable;
        };
        let Some(layout) = tab.terminal_layout() else {
            return ActiveOperationsPaneTarget::Absent;
        };
        if snapshot
            .focused_pane_id
            .as_ref()
            .is_none_or(|pane_id| !layout.contains_pane(pane_id))
        {
            return ActiveOperationsPaneTarget::Unavailable;
        }
        match self
            .active_terminal_files_target()
            .map(|(endpoint, _)| endpoint)
        {
            Some(FileEndpoint::Local { .. }) => ActiveOperationsPaneTarget::Local,
            Some(FileEndpoint::Remote { host_id, .. }) => ActiveOperationsPaneTarget::Ssh(host_id),
            _ => ActiveOperationsPaneTarget::Unavailable,
        }
    }

    /// 固定工具与完整运维页分别投影目标，右栏选择目标不改写中心页 target。
    ///
    /// @author kongweiguang
    fn tool_rail_operation_target(&self) -> ToolRailOperationTarget {
        resolve_tool_rail_operation_target(
            self.active_operations_pane_target(),
            self.selected_host_id.clone(),
        )
    }

    /// Rail 快捷项复用偏好草稿 CAS；仅真实可见工具的非居中到居中变更保留一次成功后的焦点来源。
    /// 拒绝时保留当前布局并打开原草稿；不提前聚焦，也不把配置重投影当作新的打开意图。
    ///
    /// @author kongweiguang
    pub(super) fn apply_tool_rail_context_action(
        &mut self,
        tool: ToolRailToolId,
        action: ToolRailContextAction,
        source_visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_tool_rail_placement_focus = None;
        let settings = self.current_tool_rail_settings();
        let before = self.tool_rail_state.project(&settings).panels;
        let previous_placement =
            self.preferences_panel
                .read(cx)
                .settings_snapshot()
                .map(|settings| {
                    settings
                        .tool_rail
                        .panel_placements
                        .get(&tool)
                        .copied()
                        .unwrap_or_default()
                });
        let result = self.preferences_panel.update(cx, |panel, cx| match action {
            ToolRailContextAction::MoveUp => panel.apply_tool_rail_order_change(tool, -1, cx),
            ToolRailContextAction::MoveDown => panel.apply_tool_rail_order_change(tool, 1, cx),
            ToolRailContextAction::PinToBottom => {
                panel.apply_tool_rail_bottom_change(tool, true, cx)
            }
            ToolRailContextAction::MoveToTop => {
                panel.apply_tool_rail_bottom_change(tool, false, cx)
            }
            ToolRailContextAction::Dock(placement) => {
                panel.apply_tool_rail_placement_change(tool, placement, cx)
            }
        });
        let request_focus = center_tool_placement_focus_is_requested(
            action,
            previous_placement,
            source_visible && contains_tool(before, tool) && before.center != Some(tool),
            result.is_ok(),
        );
        if let Err(message) = result {
            self.status = Some(message.into());
            self.open_preferences_modal(window, cx);
            cx.notify();
        } else if request_focus {
            self.remember_tool_rail_placement_focus(tool, window, cx);
        }
    }

    /// 菜单 handler 先于 PopupMenu.dismiss 执行；同 effect-cycle 末再捕获恢复后的焦点，不能保存菜单句柄。
    /// 受理成功才推进既有导航代次；后续导航、菜单或模态都不能被迟到保存重新聚焦。
    ///
    /// @author kongweiguang
    fn remember_tool_rail_placement_focus(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_pending_pane_focus();
        let navigation_generation = self.pane_focus_intent_generation;
        let Some(expected_focus) = window.focused(cx) else {
            return;
        };
        self.pending_tool_rail_placement_focus = Some(ToolRailPlacementFocusIntent {
            tool,
            navigation_generation,
            expected_focus,
        });
        cx.defer_in(window, move |root, window, cx| {
            if !root
                .pending_tool_rail_placement_focus
                .as_ref()
                .is_some_and(|intent| {
                    intent.tool == tool && intent.navigation_generation == navigation_generation
                })
            {
                return;
            }
            if navigation_generation != root.pane_focus_intent_generation
                || root.tool_panel_overlay_focus_is_blocked(window, cx)
            {
                root.pending_tool_rail_placement_focus = None;
                return;
            }
            if let Some(expected_focus) = window.focused(cx) {
                if let Some(intent) = root.pending_tool_rail_placement_focus.as_mut() {
                    intent.expected_focus = expected_focus;
                }
            } else {
                root.pending_tool_rail_placement_focus = None;
            }
        });
    }

    /// 明确打开已更新 RAM 投影，SettingsSaved 才确认停靠修改；两者都只消费原来源，不在 render 抢焦点。
    /// 平台先运行 next-frame 回调再 draw，因此固定两帧让新宿主先完成绘制；不是轮询，未挂载就拒绝。
    ///
    /// @author kongweiguang
    pub(super) fn complete_tool_rail_placement_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(intent) = self.pending_tool_rail_placement_focus.as_ref() else {
            return;
        };
        let tool = intent.tool;
        let navigation_generation = intent.navigation_generation;
        cx.on_next_frame(window, move |root, window, cx| {
            if !root
                .pending_tool_rail_placement_focus
                .as_ref()
                .is_some_and(|intent| {
                    intent.tool == tool && intent.navigation_generation == navigation_generation
                })
            {
                return;
            }
            if navigation_generation != root.pane_focus_intent_generation
                || root
                    .pending_tool_rail_placement_focus
                    .as_ref()
                    .is_none_or(|intent| {
                        window.focused(cx).as_ref() != Some(&intent.expected_focus)
                    })
                || root.tool_panel_overlay_focus_is_blocked(window, cx)
            {
                root.pending_tool_rail_placement_focus = None;
                return;
            }
            cx.on_next_frame(window, move |root, window, cx| {
                if !root
                    .pending_tool_rail_placement_focus
                    .as_ref()
                    .is_some_and(|intent| {
                        intent.tool == tool && intent.navigation_generation == navigation_generation
                    })
                {
                    return;
                }
                let Some(intent) = root.pending_tool_rail_placement_focus.take() else {
                    return;
                };
                let settings = root.current_tool_rail_settings();
                let panels = root.tool_rail_state.project(&settings).panels;
                let center_host_mounted = window.window_handle() == root.window_handle
                    && !root.compact_host_drawer_open
                    && !root.compact_tool_drawer_open
                    && root
                        .focus_handle
                        .contains(&root.compact_drawer_focus_handle, window);
                if center_tool_placement_focus_is_current(
                    navigation_generation,
                    root.pane_focus_intent_generation,
                    window.focused(cx).as_ref() == Some(&intent.expected_focus),
                    panels.center == Some(tool),
                    center_host_mounted,
                    root.tool_panel_overlay_focus_is_blocked(window, cx),
                ) && !root
                    .compact_drawer_focus_handle
                    .contains_focused(window, cx)
                {
                    root.clear_pending_pane_focus();
                    window.focus(&root.compact_drawer_focus_handle, cx);
                }
            });
        });
    }

    /// 浏览实体只隔离目录、选择和请求代次；文件服务、传输队列与执行器仍是组合根的同一实例。
    ///
    /// @author kongweiguang
    pub(super) fn new_files_panel(
        services: &AppServices,
        cx: &mut Context<Self>,
    ) -> Option<Entity<crate::features::files::FilesPane>> {
        let files = services.files.clone()?;
        let transfers = services.transfers.clone()?;
        let endpoint = services.initial_file_endpoint.clone()?;
        let executor = services.executor.clone();
        let targets = crate::features::files::FileTargetNameLookup::new(
            services.hosts.clone(),
            services.operations.clone(),
        );
        Some(cx.new(|_| {
            crate::features::files::FilesPane::new(files, transfers, executor, targets, endpoint)
        }))
    }

    /// Root 子模块共用确认快照中的 Rail 投影；未载入时沿用默认，不建立另一份布局 owner。
    ///
    /// @author kongweiguang
    pub(super) fn current_tool_rail_settings(&self) -> ToolRailSettings {
        self.settings_snapshot
            .as_ref()
            .map(|snapshot| snapshot.tool_rail.clone())
            .unwrap_or_default()
    }

    /// 切换宿主沿用显式宽度；预演槽位变化以便在 Agent Dock 隐藏前同步撤销搜索定位提交门。
    /// 用户操作也会取消迟到 Agent 首帧焦点；只有当前面板实际隐藏时才恢复工作区输入焦点。
    /// 显式居中打开必须等真实宿主挂载，不能聚焦 compact 隐形节点。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_tool_rail_panel(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_tool_rail_placement_focus = None;
        // 明确切换到任意工具代表用户离开当前 Agent 首帧；迟到 Ready 不能再抢回输入焦点。
        self.pending_agent_focus_intent = None;
        let settings = self.current_tool_rail_settings();
        let before = self.tool_rail_state.project(&settings);

        // 贴靠右侧的面板在 Dock 折叠时，点击表示重新展开而非误关当前面板。
        if before.panels.attached == Some(tool) && self.right_dock_collapsed {
            self.right_dock_collapsed = false;
            if tool == ToolRailToolId::Sftp {
                self.try_bind_files_from_active_terminal(cx);
            }
            cx.notify();
            return;
        }

        if before.panels.attached == Some(ToolRailToolId::AgentLauncher) {
            let mut projected_state = self.tool_rail_state.clone();
            projected_state.toggle(&settings, tool);
            if projected_state.project(&settings).panels.attached
                != Some(ToolRailToolId::AgentLauncher)
            {
                self.reconcile_agent_search_visibility(false, cx);
            }
        }
        self.tool_rail_state.toggle(&settings, tool);
        let after = self.tool_rail_state.project(&settings);
        if before.panels.attached != after.panels.attached {
            let next_default = after
                .panels
                .attached
                .map(|next_tool| self.current_attached_default_width(next_tool));
            self.tool_rail_state
                .set_attached_tool(after.panels.attached, next_default);
            self.prepare_attached_dock_width_restore(after.panels.attached);
        }
        if after.panels.attached == Some(tool) {
            self.right_dock_collapsed = false;
        }
        if tool == ToolRailToolId::Snippets
            && contains_tool(after.panels, tool)
            && let Some(commands) = self.commands_view.clone()
        {
            commands.update(cx, |commands, cx| commands.select_snippets(cx));
        }
        if tool == ToolRailToolId::Sftp && contains_tool(after.panels, tool) {
            self.try_bind_files_from_active_terminal(cx);
        }
        if contains_tool(before.panels, tool) && !contains_tool(after.panels, tool) {
            self.focus_workspace_after_hiding_tool_panel(window, cx);
        } else if center_tool_panel_focus_on_open(
            before.panels,
            after.panels,
            tool,
            self.tool_panel_overlay_focus_is_blocked(window, cx),
        ) {
            self.remember_tool_rail_placement_focus(tool, window, cx);
            self.complete_tool_rail_placement_focus(window, cx);
        }
        cx.notify();
    }

    /// 系统提示、上层模态和菜单先拥有 Escape；仅显式工具打开可接管正文焦点，不在重绘时迁移编辑焦点。
    ///
    /// @author kongweiguang
    pub(super) fn tool_panel_overlay_focus_is_blocked(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> bool {
        self.tool_panel_focus_is_blocked_except_snippet_editor(window, cx, false)
    }

    /// 仅已核对的 Fill 完成回执可接管原片段弹层；其它模态和系统提示仍拥有焦点。
    /// 同时要求焦点仍在原弹层的 focus tree，不能用该豁免穿过后来打开的菜单。
    ///
    /// @author kongweiguang
    pub(super) fn tool_panel_focus_is_blocked_except_snippet_editor(
        &self,
        window: &Window,
        cx: &Context<Self>,
        allow_snippet_editor: bool,
    ) -> bool {
        let snippet_editor_owns_focus = allow_snippet_editor
            && self.snippet_editor_modal_open
            && window.focused(cx).is_some_and(|focused| {
                focused == self.snippet_editor_modal_focus_handle
                    || self
                        .snippet_editor_modal_focus_handle
                        .contains(&focused, window)
            });
        !window.is_window_active()
            || window.has_active_prompt()
            || self.host_shutdown_requested
            || self.connection_prompt_modal_open
            || self.split_target_picker.read(cx).is_open()
            || logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                (self.snippet_editor_modal_open && !snippet_editor_owns_focus)
                    || self.custom_agent_manager.is_some(),
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some() || self.quit_ready_intent.is_some(),
            )
            || (keybinding_focus::classify_focus_scope(&window.context_stack(), false)
                == KeybindingFocusScope::Modal
                && !snippet_editor_owns_focus)
    }

    /// X、遮罩与冒泡 Escape 共用精确居中 owner；只隐藏现有 Entity，不取消其任务或丢弃编辑草稿。
    /// 宽窄切换、上层确认/菜单和迟到回调都不能转投其它工具；原关闭路径继续恢复正文焦点。
    ///
    /// @author kongweiguang
    pub(super) fn dismiss_center_tool_panel(
        &mut self,
        rendered_tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        if center_tool_panel_close_is_current(
            rendered_tool,
            panels,
            self.tool_rail_rendered_compact.0,
            self.tool_panel_overlay_focus_is_blocked(window, cx),
        ) {
            self.toggle_tool_rail_panel(rendered_tool, window, cx);
        }
    }

    /// 用户关闭与自动响应式卸载分开：Agent 抽屉关闭先同步取消定位，再只移除精确工具并复用焦点返回口。
    /// 不经过 toggle 的折叠 Dock 展开分支，不写位置/宽度，也不销毁其它工具任务或草稿。
    ///
    /// @author kongweiguang
    pub(super) fn dismiss_compact_drawer(
        &mut self,
        rendered_tool: Option<ToolRailToolId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !compact_drawer_close_is_current(
            rendered_tool,
            self.tool_rail_rendered_compact,
            (self.compact_host_drawer_open, self.compact_tool_drawer_open),
            self.tool_panel_overlay_focus_is_blocked(window, cx),
        ) {
            return;
        }
        if rendered_tool == Some(ToolRailToolId::AgentLauncher) && self.compact_tool_drawer_open {
            self.reconcile_agent_search_visibility(false, cx);
        }
        if let Some(tool) = rendered_tool {
            let settings = self.current_tool_rail_settings();
            let before = self.tool_rail_state.project(&settings).panels;
            if !self.tool_rail_state.close(&settings, tool) {
                return;
            }
            let after = self.tool_rail_state.project(&settings).panels;
            if before.attached != after.attached {
                let next_default = after
                    .attached
                    .map(|next_tool| self.current_attached_default_width(next_tool));
                self.tool_rail_state
                    .set_attached_tool(after.attached, next_default);
                self.prepare_attached_dock_width_restore(after.attached);
            }
        }
        self.clear_pending_pane_focus();
        self.pending_tool_rail_placement_focus = None;
        self.close_compact_drawer(window, cx);
    }

    /// 居中宿主只处理子控件未消费的 Escape；旧宿主回调不能关闭后来替换的工具，关闭仍走既有焦点返回口。
    ///
    /// @author kongweiguang
    pub(super) fn handle_center_tool_panel_key_down(
        &mut self,
        rendered_tool: ToolRailToolId,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        if !self.tool_rail_rendered_compact.0
            && let Some(tool) = center_tool_panel_escape_target(
                &event.keystroke.key,
                rendered_tool,
                panels,
                self.compact_drawer_focus_handle
                    .contains_focused(window, cx),
                self.tool_panel_overlay_focus_is_blocked(window, cx),
            )
        {
            cx.stop_propagation();
            self.dismiss_center_tool_panel(tool, window, cx);
        }
    }

    /// 只关闭当前真实 compact 宿主；None 固定代表主机，Some 固定代表绘制时捕获的工具，不猜活动目标。
    /// 冒泡保证子控件先消费 Escape；显式工具关闭移除运行时打开项，自动收缩仍保留原状态与保存布局。
    ///
    /// @author kongweiguang
    pub(super) fn handle_compact_drawer_key_down(
        &mut self,
        rendered_tool: Option<ToolRailToolId>,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if compact_drawer_escape_is_current(
            &event.keystroke.key,
            rendered_tool,
            self.tool_rail_rendered_compact,
            (self.compact_host_drawer_open, self.compact_tool_drawer_open),
            self.compact_drawer_focus_handle
                .contains_focused(window, cx),
            self.tool_panel_overlay_focus_is_blocked(window, cx),
        ) {
            cx.stop_propagation();
            self.dismiss_compact_drawer(rendered_tool, window, cx);
        }
    }

    /// 隐藏工具后回到当前正文输入目标；复用 pane 的绑定/代次门禁，不重新激活 Tab 或更换 Agent 目标。
    /// 模态和系统提示仍拥有焦点；没有可输入正文时才使用常驻工作区焦点接收导航快捷键。
    ///
    /// @author kongweiguang
    fn focus_workspace_after_hiding_tool_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_prompt()
            || logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                self.snippet_editor_modal_open || self.custom_agent_manager.is_some(),
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some(),
            )
            || keybinding_focus::classify_focus_scope(&window.context_stack(), false)
                == KeybindingFocusScope::Modal
        {
            return;
        }

        self.clear_pending_pane_focus();
        self.focused_agent_terminal = false;
        if self.center_panel == CenterPanel::Terminal {
            let target = self.workspace_controller.as_ref().and_then(|controller| {
                let snapshot = controller.snapshot();
                Some((
                    snapshot.active_tab_id.clone()?,
                    focused_workspace_pane(snapshot)?,
                ))
            });
            if let Some((tab_id, pane_id)) = target {
                self.request_workspace_pane_focus(tab_id, pane_id, window, cx);
                return;
            }
        }

        self.set_reported_focus_session(None, cx);
        let editor_focused = self.center_panel == CenterPanel::Editor
            && self.file_editor_workspace.clone().is_some_and(|editor| {
                editor.update(cx, |workspace, cx| {
                    workspace.focus_active_editor(window, cx)
                })
            });
        if !editor_focused {
            window.focus(&self.focus_handle, cx);
        }
        self.cancel_unfocused_pane_compositions(window, cx);
    }

    /// 仅投影当前 Ready、精确 UI owner 的终端/编辑器来源 pane；有效 OSC7 目录优先于启动默认值。
    /// 目录只来自通过 session/revision 门禁的缓存，resize 清画面不使文件面板跳回旧目录。
    ///
    /// @author kongweiguang
    fn active_terminal_files_target(&self) -> Option<(FileEndpoint, Option<ExternalTargetLease>)> {
        if !center_preserves_terminal_file_context(self.center_panel) {
            return None;
        }
        let controller = self.workspace_controller.as_ref()?;
        let snapshot = controller.snapshot();
        let tab_id = snapshot.active_tab_id.as_ref()?;
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let layout = tab.terminal_layout()?;
        let pane_id = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| layout.contains_pane(pane_id))?;
        let pane = self.workspace_runtime.pane(pane_id)?;
        if pane.tab_id != *tab_id || pane.phase != RuntimePanePhase::Ready {
            return None;
        }
        let session_id = pane.session_id.as_ref()?;
        let binding = &self.pane_input_handlers.get(session_id)?.binding;
        if !ui_binding_matches_pane(binding, session_id, tab_id, pane_id) {
            return None;
        }
        if let Some(lease) = pane.external_lease.as_ref()
            && !matches!(
                pane.target.as_ref(),
                Some(SessionTarget::Ssh(ssh)) if &ssh.host_id == lease.host_id()
            )
        {
            return None;
        }
        let target = pane.target.as_ref()?;
        let current_cwd = pane
            .current_working_directory
            .as_deref()
            .or(pane.persisted.current_cwd.as_deref());
        let endpoint = match target {
            SessionTarget::Ssh(_) => ssh_files_endpoint(
                target,
                pane.persisted.target.as_ref(),
                pane.persisted.remote_host_id.as_ref(),
                current_cwd,
                pane.persisted.cwd.as_deref(),
            )?,
            SessionTarget::Local(_) => local_files_endpoint(
                target,
                pane.persisted.target.as_ref(),
                pane.persisted.remote_host_id.as_ref(),
                pane.persisted.profile_id.as_deref(),
                current_cwd,
                pane.persisted.cwd.as_deref(),
                self.services.initial_file_endpoint.as_ref(),
            )?,
            _ => return None,
        };
        let lease = pane.external_lease.clone();
        Some((endpoint, lease))
    }

    /// 同步右栏文件面板当前可信 pane；切换 Tab 只更新文件显示目标，不触碰 Agent 绑定。
    ///
    /// @author kongweiguang
    fn try_bind_files_from_active_terminal(&mut self, cx: &mut Context<Self>) {
        let (endpoint, lease) = self
            .active_terminal_files_target()
            .map(|(endpoint, lease)| (Some(endpoint), lease))
            .unwrap_or((None, None));
        if let Some(files) = self.sftp_panel.clone() {
            files.update(cx, move |files, cx| {
                files.set_current_terminal_target(endpoint, lease, cx);
            });
        }
    }

    /// 配置 CAS 成功后按新设置投影并恢复目标工具的显式宽度，不把重排后的受限布局当用户意图。
    ///
    /// @author kongweiguang
    pub(super) fn remap_tool_rail_settings(&mut self, cx: &mut Context<Self>) {
        let settings = self.current_tool_rail_settings();
        let previous_tool = self.tool_rail_state.attached_tool();
        let layout = self.tool_rail_state.project(&settings);
        if previous_tool != layout.panels.attached {
            let next_default = layout
                .panels
                .attached
                .map(|next_tool| self.current_attached_default_width(next_tool));
            self.tool_rail_state
                .set_attached_tool(layout.panels.attached, next_default);
            self.prepare_attached_dock_width_restore(layout.panels.attached);
        }
        cx.notify();
    }

    /// 只持久化当前工具的明确用户拖动内容宽度，去掉 4px 槽；动态默认/程序恢复从不走此入口。
    ///
    /// @author kongweiguang
    pub(super) fn record_user_attached_width(
        &mut self,
        tool: ToolRailToolId,
        width: gpui_kit::Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some(content) = super::workbench::workbench_content_dimension(
            width,
            super::workbench::RESIZABLE_EDGE_GUTTER,
            260,
            680,
        ) else {
            return;
        };
        if self.right_dock_collapsed || !self.tool_rail_state.remember_user_resize(tool, width) {
            return;
        }

        self.workbench_layout_restore_generation =
            self.workbench_layout_restore_generation.wrapping_add(1);
        self.workbench_dock_restore_pending = false;
        self.workbench_dock_restore_width = None;
        let mut layout = self.current_workbench_layout_intent();
        if self.tool_rail_state.attached_width(tool).is_some() {
            layout.dock_panel_widths.insert(tool, content);
        } else {
            layout.dock_panel_widths.remove(&tool);
        }
        self.request_workbench_layout_intent(layout, cx);
        cx.notify();
    }

    /// 仅从当前确定绑定的内容状态选择 Agent 默认宽度；激活与绘制共用此只读规则，不能猜两套预算。
    ///
    /// @author kongweiguang
    pub(super) fn current_attached_default_width(&self, tool: ToolRailToolId) -> gpui_kit::Pixels {
        attached_dock_default_width(tool, self.agent_terminal_content_available())
    }

    /// 为下一帧登记当前工具目标并增加 generation，使快速切换时过期 resize 回调失效。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_attached_dock_width_restore(&mut self, tool: Option<ToolRailToolId>) {
        self.workbench_layout_restore_generation =
            self.workbench_layout_restore_generation.wrapping_add(1);
        if let Some(tool) = tool {
            let default = self.current_attached_default_width(tool);
            self.workbench_dock_restore_width =
                Some(self.tool_rail_state.attached_width(tool).unwrap_or(default));
            self.workbench_dock_restore_pending = true;
        } else {
            self.workbench_dock_restore_width = None;
            self.workbench_dock_restore_pending = false;
        }
    }

    /// 与工作台 Agent 终端装配使用相同的四项绑定条件，避免用过期 Agent 选择推导运行态默认宽度。
    ///
    /// @author kongweiguang
    fn agent_terminal_content_available(&self) -> bool {
        self.agent_terminal_session_id.is_some()
            && self.agent_terminal_snapshot.is_some()
            && self.agent_terminal_tools.is_some()
            && self.selected_agent_session_id.is_some()
    }

    /// 仅为可见工具装配业务内容；SFTP 使用独立浏览实体，避免当前终端绑定污染中心文件工作区。
    ///
    /// @author kongweiguang
    pub(super) fn workbench_tool_panels(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WorkbenchToolPanels {
        let settings = self.current_tool_rail_settings();
        let layout = self.tool_rail_state.project(&settings);
        let theme = ThemeTokens::current(window, cx);
        let window_active = window.is_window_active();

        let active_operations = self.active_operations_tools(layout.panels);
        for (tool, section) in active_operations {
            let _ = self.ensure_operations_panel(tool, section, window, cx);
        }
        self.refresh_operations_panel_targets(cx);
        self.sync_shared_panel_activity(layout.panels, window_active, cx);
        let is_compact_slot =
            |tool| layout.panels.attached == Some(tool) || layout.panels.left == Some(tool);
        if let Some(files_pane) = self.sftp_panel.clone() {
            files_pane.update(cx, |files_pane, cx| {
                // 此实体始终表示当前终端的单端 SFTP；停靠位置不应把它变成完整双端工作区。
                files_pane.set_compact_layout(true, cx);
            });
        }
        if contains_tool(layout.panels, ToolRailToolId::Sftp) {
            self.try_bind_files_from_active_terminal(cx);
        }
        let snippets_compact = is_compact_slot(ToolRailToolId::Snippets);
        if let Some(commands_view) = self.commands_view.clone() {
            commands_view.update(cx, |commands_view, cx| {
                commands_view.set_compact_layout(snippets_compact, cx);
            });
        }

        let renderers = WorkbenchToolPanelRenderers {
            agent_launcher: self.agents_panel.clone().map_or_else(
                || {
                    unavailable_panel(
                        "tool-agent-launcher-unavailable",
                        "Agent 服务尚未装配。",
                        "The Agent service is unavailable.",
                        theme,
                    )
                },
                |entity| entity.into_any_element(),
            ),
            sftp: self.sftp_panel.clone().map_or_else(
                || {
                    unavailable_panel(
                        "tool-sftp-unavailable",
                        "文件与传输服务尚未装配。",
                        "File and transfer services are unavailable.",
                        theme,
                    )
                },
                |entity| entity.into_any_element(),
            ),
            containers: self.operations_panel_renderer(ToolRailToolId::Containers, theme),
            snippets: self.commands_view.clone().map_or_else(
                || {
                    unavailable_panel(
                        "tool-snippets-unavailable",
                        "命令服务尚未装配。",
                        "Command services are unavailable.",
                        theme,
                    )
                },
                |entity| entity.into_any_element(),
            ),
            tmux: self.operations_panel_renderer(ToolRailToolId::Tmux, theme),
            ports: self.operations_panel_renderer(ToolRailToolId::Ports, theme),
            system: self.operations_panel_renderer(ToolRailToolId::System, theme),
            logs: self.logs_view.clone().map_or_else(
                || {
                    unavailable_panel(
                        "tool-logs-unavailable",
                        "命令历史服务尚未装配。",
                        "Command history is unavailable.",
                        theme,
                    )
                },
                |entity| entity.into_any_element(),
            ),
        };

        WorkbenchToolPanels { layout, renderers }
    }

    /// 供中心内容构造器判断同一 Files/Commands Entity 是否已由 ToolRail 唯一托管。
    ///
    /// @author kongweiguang
    pub(super) fn tool_rail_owns_center_panel(&mut self) -> bool {
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        let (tool, center_entity_is_shared) = match self.center_panel {
            CenterPanel::Commands => (Some(ToolRailToolId::Snippets), self.commands_view.is_some()),
            _ => (None, false),
        };
        center_route_uses_tool_rail_entity(
            self.center_panel,
            tool.is_some_and(|tool| self.tool_rail_panel_visible(panels, tool)),
            center_entity_is_shared,
        )
    }

    /// workspace 或 Agent selection 已稳定变化时更新 Context 投影，避免每帧 set_projection 通知。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_context_panel_projection(&mut self, cx: &mut Context<Self>) {
        let projection = self.context_workspace_projection();
        let selected_agent = self.selected_agent_session_id.clone();
        if let Some(panel) = self.context_panel.clone() {
            panel.update(cx, move |panel, cx| {
                panel.set_projection(projection, cx);
                panel.set_selected_agent(selected_agent, cx);
            });
        }
    }

    /// 显式主题切换时刷新固定区运维面板，避免每帧 render 都触发子面板 notify。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_tool_rail_panel_theme(&mut self, cx: &mut Context<Self>) {
        let theme_mode = self.theme_mode;
        let views = self
            .tool_rail_operations_views
            .values()
            .cloned()
            .chain(self.operations_view.iter().cloned())
            .collect::<Vec<_>>();
        for view in views {
            view.update(cx, |view, cx| view.set_theme_mode(theme_mode, cx));
        }
    }

    /// 按需建立唯一工具视图；容器从真实 pane 初始化，危险确认固定回带原 Entity 和目标。
    ///
    /// @author kongweiguang
    fn ensure_operations_panel(
        &mut self,
        tool: ToolRailToolId,
        section: OperationsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<OperationsView>> {
        if let Some(view) = self.tool_rail_operations_views.get(&tool).cloned() {
            return Some(view);
        }
        let operations = self.services.operations.clone()?;
        let executor = self.services.executor.clone();
        let initial_tool_target = self.tool_rail_operation_target();
        let initial_container_context =
            (tool == ToolRailToolId::Containers).then(|| self.current_container_panel_context());
        let proxy_panel = (tool == ToolRailToolId::Ports && operations.terminal_proxy().is_some())
            .then(|| {
                cx.new(|_| {
                    crate::features::operations::proxy_panel::ProxyApplicationPanel::new(
                        crate::features::operations::proxy_panel::ProxyPanelProjection::default(),
                    )
                })
            });
        let proxy_subscription = proxy_panel.as_ref().map(|panel| {
            cx.subscribe_in(panel, window, |root, source, event, _, cx| {
                root.handle_proxy_panel_event(source.clone(), event, cx);
            })
        });
        let view = cx.new(|_| {
            let view = OperationsView::new_panel(operations, executor, section);
            let view = match initial_container_context {
                Some(context) => view.with_initial_container_panel_context(context),
                None => view.with_initial_tool_rail_target(initial_tool_target),
            };
            match proxy_panel.clone() {
                Some(panel) => view.with_proxy_panel(panel),
                None => view,
            }
        });
        let theme_mode = self.theme_mode;
        view.update(cx, |view, cx| view.set_theme_mode(theme_mode, cx));
        let subscription = cx.subscribe_in(&view, window, |root, source, event, window, cx| {
            let panes_before = root
                .workspace_runtime
                .panes()
                .map(|(pane_id, _)| pane_id.clone())
                .collect::<Vec<_>>();
            if let OperationsEvent::RootConfirmationChanged = event {
                let active = source.read(cx).root_confirmation_active();
                root.sync_operations_root_confirmation(source.clone(), active, window, cx);
            } else if let OperationsEvent::ContainerExecRequested { target, .. } = event {
                let target = target.clone();
                super::operations::handle_operations_event(root, event, source, window, cx);
                if let Some((pane_id, _)) =
                    root.workspace_runtime.panes().find(|(pane_id, pane)| {
                        !panes_before.contains(pane_id)
                            && pane.phase == RuntimePanePhase::RestorePending
                    })
                {
                    root.container_exec_refresh_targets.insert(
                        pane_id.clone(),
                        super::ContainerExecRefreshTarget {
                            operations: source.clone(),
                            host_id: target.host_id,
                            runtime: target.runtime,
                        },
                    );
                }
            } else {
                super::operations::handle_operations_event(root, event, source, window, cx);
            }
        });
        self.tool_rail_operations_views.insert(tool, view.clone());
        self._tool_rail_operations_subscriptions
            .insert(tool, subscription);
        if let (Some(panel), Some(subscription)) = (proxy_panel, proxy_subscription) {
            self.tool_rail_proxy_panels.insert(tool, panel);
            self._tool_rail_proxy_panel_subscriptions
                .insert(tool, subscription);
            self.refresh_proxy_panel(cx);
        }
        Some(view)
    }

    /// 把既有运维 Entity 转成固定分区 renderer；服务缺失时给出明确不可用原因。
    ///
    /// @author kongweiguang
    fn operations_panel_renderer(&self, tool: ToolRailToolId, theme: ThemeTokens) -> AnyElement {
        self.tool_rail_operations_views
            .get(&tool)
            .cloned()
            .map_or_else(
                || {
                    unavailable_panel(
                        operations_unavailable_id(tool),
                        "运维服务尚未装配。",
                        "Operations services are unavailable.",
                        theme,
                    )
                },
                |entity| entity.into_any_element(),
            )
    }

    /// 容器使用精确 session 上下文，其它工具保留各自目标规则；侧栏选择不能改写容器宿主。
    /// 同源名称只用于展示，绑定和异步代次仍使用稳定身份。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_operations_panel_targets(&mut self, cx: &mut Context<Self>) {
        let tmux_binding = super::operations::focused_tmux_quickref_binding(self);
        let tmux_attach_binding = super::operations::focused_tmux_attach_binding(self);
        let fixed_target = self.tool_rail_operation_target();
        let container_context = self.current_container_panel_context();
        let host_display_names = self
            .host_navigation
            .as_ref()
            .map(|navigation| navigation.read(cx).display_names())
            .unwrap_or_default();
        if let Some(view) = self.logs_view.clone() {
            view.update(cx, |view, cx| {
                view.set_host_display_names(host_display_names.clone(), cx);
            });
        }
        let views = self
            .tool_rail_operations_views
            .iter()
            .map(|(tool, view)| {
                (
                    view.clone(),
                    Some(fixed_target.clone()),
                    *tool == ToolRailToolId::Containers,
                )
            })
            .chain(
                self.operations_view
                    .iter()
                    .cloned()
                    .map(|view| (view, None, false)),
            )
            .collect::<Vec<_>>();
        let selected_host_id = self.selected_host_id.clone();
        for (view, fixed_target, is_container) in views {
            view.update(cx, |view, cx| {
                view.set_host_display_names(host_display_names.clone(), cx);
                if is_container {
                    view.set_container_panel_context(container_context.clone(), cx);
                } else if let Some(target) = fixed_target {
                    view.set_tool_rail_target(target, cx);
                } else {
                    view.set_host_target(selected_host_id.clone(), cx);
                }
                view.set_tmux_quickref_binding(tmux_binding.clone(), cx);
                view.set_tmux_attach_binding(tmux_attach_binding.clone(), cx);
            });
        }
    }

    /// 按真实槽位同步运维、Context 和 Logs 面板的可见状态，运维紧凑度仅由 Attached/Left 决定。
    ///
    /// @author kongweiguang
    fn sync_shared_panel_activity(
        &mut self,
        panels: ToolRailPanelSlots,
        window_active: bool,
        cx: &mut Context<Self>,
    ) {
        let operation_views = self
            .tool_rail_operations_views
            .iter()
            .map(|(tool, view)| (*tool, view.clone()))
            .collect::<Vec<_>>();
        for (tool, view) in operation_views {
            let active =
                tool_rail_panel_is_active(panels, self.right_dock_collapsed, window_active, tool);
            let compact = panels.attached == Some(tool) || panels.left == Some(tool);
            view.update(cx, |view, cx| {
                view.set_compact_layout(compact, cx);
                view.set_active(active, cx);
            });
        }

        let context_active = tool_rail_panel_is_active(
            panels,
            self.right_dock_collapsed,
            window_active,
            ToolRailToolId::Context,
        );
        if let Some(panel) = self.context_panel.clone() {
            panel.update(cx, |panel, cx| panel.set_active(context_active, cx));
        }

        let logs_active = tool_rail_panel_is_active(
            panels,
            self.right_dock_collapsed,
            window_active,
            ToolRailToolId::Logs,
        );
        let current_target = self.current_logs_target(cx);
        let theme_mode = self.theme_mode;
        if let Some(view) = self.logs_view.clone() {
            view.update(cx, |view, cx| {
                view.set_target(current_target, cx);
                view.set_theme_mode(theme_mode, cx);
                view.set_active(logs_active, cx);
            });
        }
    }

    /// 折叠的 Attached 宿主不算活动；其他槽位的唯一面板实体可继续工作。
    ///
    /// @author kongweiguang
    fn tool_rail_panel_visible(&self, panels: ToolRailPanelSlots, tool: ToolRailToolId) -> bool {
        tool_rail_panel_is_visible(panels, self.right_dock_collapsed, tool)
    }

    /// Agent 输入焦点只在 AgentLauncher 实际挂载且未被右侧 dock 折叠时可用；真实投影会更新布局缓存，因此保留可变借用。
    ///
    /// @author kongweiguang
    pub(super) fn agent_tool_panel_is_visible(&mut self) -> bool {
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        self.tool_rail_panel_visible(panels, ToolRailToolId::AgentLauncher)
    }
}

/// 编辑器只是把中心内容切换为文件视图，不代表当前终端 pane 已释放；其他中心页不延续该上下文。
///
/// @author kongweiguang
fn center_preserves_terminal_file_context(center: CenterPanel) -> bool {
    matches!(center, CenterPanel::Terminal | CenterPanel::Editor)
}

/// 仅接受运行态与持久化都指向同一 SSH host 的 pane，并按最近工作目录优先构造 SFTP endpoint。
///
/// @author kongweiguang
fn ssh_files_endpoint(
    target: &SessionTarget,
    persisted_target: Option<&WorkspaceTargetRef>,
    persisted_remote_host_id: Option<&HostId>,
    current_cwd: Option<&str>,
    cwd: Option<&str>,
) -> Option<FileEndpoint> {
    let SessionTarget::Ssh(ssh) = target else {
        return None;
    };
    let WorkspaceTargetRef::Ssh { host_id } = persisted_target? else {
        return None;
    };
    if host_id != &ssh.host_id
        || persisted_remote_host_id.is_some_and(|persisted| persisted != host_id)
    {
        return None;
    }
    let target_cwd = ssh
        .working_directory
        .as_deref()
        .and_then(|path| path.to_str());
    [current_cwd, cwd, target_cwd, Some(".")]
        .into_iter()
        .flatten()
        .find_map(|path| FileEndpoint::remote(host_id.clone(), path).ok())
}

/// 只接受当前 pane 的本机 target；起始目录只作为没有工作区 CWD 与 Profile CWD 时的一次性 fallback。
///
/// @author kongweiguang
fn local_files_endpoint(
    target: &SessionTarget,
    persisted_target: Option<&WorkspaceTargetRef>,
    persisted_remote_host_id: Option<&HostId>,
    persisted_profile_id: Option<&str>,
    current_cwd: Option<&str>,
    cwd: Option<&str>,
    initial_file_endpoint: Option<&FileEndpoint>,
) -> Option<FileEndpoint> {
    let SessionTarget::Local(local) = target else {
        return None;
    };
    let WorkspaceTargetRef::Local { profile_id } = persisted_target? else {
        return None;
    };
    if persisted_remote_host_id.is_some()
        || profile_id
            .as_deref()
            .zip(persisted_profile_id)
            .is_some_and(|(target_profile, pane_profile)| target_profile != pane_profile)
    {
        return None;
    }

    let persisted_directory = [current_cwd, cwd]
        .into_iter()
        .flatten()
        .find(|path| !path.is_empty())
        .map(PathBuf::from);
    let configured_directory = match initial_file_endpoint {
        Some(FileEndpoint::Local { path }) => Some(path.clone()),
        Some(FileEndpoint::Remote { .. } | FileEndpoint::Container { .. }) | None => None,
    };
    persisted_directory
        .or_else(|| local.working_directory.clone())
        .or(configured_directory)
        .map(|path| FileEndpoint::Local { path })
}

/// 拒绝把后台或另一 tab/pane 的 session binding 当成活动 UI terminal。
///
/// @author kongweiguang
fn ui_binding_matches_pane(
    binding: &UiSessionBinding,
    session_id: &kerminal_domain::identity::SessionId,
    tab_id: &kerminal_domain::identity::TabId,
    pane_id: &kerminal_domain::identity::PaneId,
) -> bool {
    binding.session_id == *session_id
        && binding.owner
            == (SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            })
}

/// 将独立 Tools slot 与 main Commands/Files route 中会重复挂载的实体身份对比。
///
/// @author kongweiguang
fn panels_has_tool(panels: ToolRailPanelSlots, tool: ToolRailToolId) -> bool {
    panels.attached == Some(tool)
        || panels.left == Some(tool)
        || panels.bottom == Some(tool)
        || panels.center == Some(tool)
}

/// 用当前 projection 检查是否已打开，避免单独保存第二份可见状态。
///
/// @author kongweiguang
fn contains_tool(panels: ToolRailPanelSlots, tool: ToolRailToolId) -> bool {
    panels_has_tool(panels, tool)
}

/// 只有中心和已显示的工具槽会重复消费同一 Entity 时才替换中心 renderer。
///
/// @author kongweiguang
fn center_route_uses_tool_rail_entity(
    center: CenterPanel,
    tool_rail_visible: bool,
    center_entity_is_shared: bool,
) -> bool {
    matches!(center, CenterPanel::Commands | CenterPanel::Files)
        && tool_rail_visible
        && center_entity_is_shared
}

/// 仅将真实挂载且未折叠的槽标记为可见，供轮询与读取任务遵守宿主生命周期。
///
/// @author kongweiguang
fn tool_rail_panel_is_visible(
    panels: ToolRailPanelSlots,
    right_dock_collapsed: bool,
    tool: ToolRailToolId,
) -> bool {
    if panels.attached == Some(tool) {
        return !right_dock_collapsed;
    }
    panels.left == Some(tool) || panels.bottom == Some(tool) || panels.center == Some(tool)
}

/// 只有窗口聚焦且唯一宿主实际显示时才运行工具轮询，后台窗口不会维持前台采样频率。
///
/// @author kongweiguang
fn tool_rail_panel_is_active(
    panels: ToolRailPanelSlots,
    right_dock_collapsed: bool,
    window_active: bool,
    tool: ToolRailToolId,
) -> bool {
    window_active && tool_rail_panel_is_visible(panels, right_dock_collapsed, tool)
}

impl RootView {
    /// 活动运维分区分别使用独立 Entity；折叠的右侧宿主不启动隐藏查询。
    ///
    /// @author kongweiguang
    fn active_operations_tools(
        &self,
        panels: ToolRailPanelSlots,
    ) -> Vec<(ToolRailToolId, OperationsSection)> {
        [
            (ToolRailToolId::Containers, OperationsSection::Containers),
            (ToolRailToolId::Tmux, OperationsSection::Tmux),
            (ToolRailToolId::Ports, OperationsSection::PortForwards),
            (ToolRailToolId::System, OperationsSection::ServerInfo),
        ]
        .into_iter()
        .filter(|(tool, _)| {
            panels_has_tool(panels, *tool) && self.tool_rail_panel_visible(panels, *tool)
        })
        .collect()
    }
}

/// 复用 compact 的临时焦点句柄仅限正常居中宿主；compact_width 必然蕴含 compact_panels，两个抽屉也不会并存。
///
/// @author kongweiguang
pub(super) fn expanded_center_tool_panel(
    panels: ToolRailPanelSlots,
    compact_panels: bool,
) -> Option<ToolRailToolId> {
    if compact_panels { None } else { panels.center }
}

/// 仅首次显式打开或替换 center 工具才迁移焦点；关闭、普通重绘和其它停靠位置不改变正在编辑的控件。
///
/// @author kongweiguang
fn center_tool_panel_focus_on_open(
    before: ToolRailPanelSlots,
    after: ToolRailPanelSlots,
    tool: ToolRailToolId,
    focus_blocked: bool,
) -> bool {
    !focus_blocked && after.center == Some(tool) && before.center != after.center
}

/// 保存受理不能等同焦点提交；只允许真实可见且确实从其它位置显式改到 Center 的原工具登记来源。
///
/// @author kongweiguang
fn center_tool_placement_focus_is_requested(
    action: ToolRailContextAction,
    previous_placement: Option<ToolRailPanelPlacement>,
    source_visible: bool,
    save_accepted: bool,
) -> bool {
    save_accepted
        && source_visible
        && matches!(
            action,
            ToolRailContextAction::Dock(ToolRailPanelPlacement::Center)
        )
        && previous_placement.is_some_and(|placement| placement != ToolRailPanelPlacement::Center)
}

/// 成功回执只允许一次原来源交接；后来的导航/物理焦点、更高模态、替换 owner 或未挂载宿主一律拒绝。
///
/// @author kongweiguang
fn center_tool_placement_focus_is_current(
    requested_generation: u64,
    current_generation: u64,
    focus_unchanged: bool,
    center_owner_matches: bool,
    center_host_mounted: bool,
    focus_blocked: bool,
) -> bool {
    requested_generation == current_generation
        && focus_unchanged
        && center_owner_matches
        && center_host_mounted
        && !focus_blocked
}

/// 主机与工具共用焦点句柄，故必须同时核对真实 compact 投影和互斥打开身份，旧工具回调不转投新 owner。
/// None 是固定主机身份而不是“任意工具”；菜单、模态与失焦沿用 Root 的现有 fence。
///
/// @author kongweiguang
fn compact_drawer_escape_is_current(
    key: &str,
    rendered_tool: Option<ToolRailToolId>,
    compact_projection: (bool, Option<ToolRailToolId>),
    drawers_open: (bool, bool),
    focus_in_surface: bool,
    focus_blocked: bool,
) -> bool {
    key.eq_ignore_ascii_case("escape")
        && focus_in_surface
        && compact_drawer_close_is_current(
            rendered_tool,
            compact_projection,
            drawers_open,
            focus_blocked,
        )
}

/// 点击 X/遮罩不要求旧焦点仍在正文，但必须是当帧真实 compact 宿主；上层菜单与确认继续拥有输入。
///
/// @author kongweiguang
fn compact_drawer_close_is_current(
    rendered_tool: Option<ToolRailToolId>,
    compact_projection: (bool, Option<ToolRailToolId>),
    drawers_open: (bool, bool),
    focus_blocked: bool,
) -> bool {
    if !compact_projection.0 || focus_blocked {
        return false;
    }
    match rendered_tool {
        Some(tool) => drawers_open == (false, true) && compact_projection.1 == Some(tool),
        None => drawers_open == (true, false),
    }
}

/// 显式居中关闭只接受仍挂载的原工具；旧宽窗回调不可关闭已转入 compact 的共享业务实体。
///
/// @author kongweiguang
fn center_tool_panel_close_is_current(
    rendered_tool: ToolRailToolId,
    panels: ToolRailPanelSlots,
    compact_panels: bool,
    focus_blocked: bool,
) -> bool {
    expanded_center_tool_panel(panels, compact_panels) == Some(rendered_tool) && !focus_blocked
}

/// 只允许实际居中且仍拥有物理焦点的原宿主关闭；重复 Escape、旧工具回调或更高层输入 owner 均不转投目标。
///
/// @author kongweiguang
fn center_tool_panel_escape_target(
    key: &str,
    rendered_tool: ToolRailToolId,
    panels: ToolRailPanelSlots,
    focus_in_surface: bool,
    focus_blocked: bool,
) -> Option<ToolRailToolId> {
    (key.eq_ignore_ascii_case("escape")
        && panels.center == Some(rendered_tool)
        && focus_in_surface
        && !focus_blocked)
        .then_some(rendered_tool)
}

/// 不可用 renderer 明示缺少的服务；不会把未装配显示成成功空列表。
///
/// @author kongweiguang
fn unavailable_panel(
    id: &'static str,
    zh_message: &'static str,
    en_message: &'static str,
    theme: ThemeTokens,
) -> AnyElement {
    div()
        .id(id)
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(12.))
        .text_color(theme.muted)
        .child(crate::locale::text(zh_message, en_message))
        .into_any_element()
}

/// 固定运维 renderer 的控件 ID 与 ToolId 一一对应，方便原生 UI 验收定位。
///
/// @author kongweiguang
fn operations_unavailable_id(tool: ToolRailToolId) -> &'static str {
    match tool {
        ToolRailToolId::Containers => "tool-containers-unavailable",
        ToolRailToolId::Tmux => "tool-tmux-unavailable",
        ToolRailToolId::Ports => "tool-ports-unavailable",
        ToolRailToolId::System => "tool-system-unavailable",
        _ => "tool-operations-unavailable",
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_tool_panels.rs"]
mod tests;
