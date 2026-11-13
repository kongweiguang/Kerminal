//! ToolRail 面板的唯一宿主装配与工作区冲突投影。
//!
//! @author kongweiguang

use std::collections::BTreeSet;
use std::path::PathBuf;

use gpui_kit::{
    AnyElement, AnyView, AppContext as _, Context, Entity, FocusHandle, KeyDownEvent, Window,
    component::WindowExt as _, div, prelude::*, px,
};
use kerminal_application::{
    platform::external_launch::ExternalTargetLease,
    plugins::{
        PluginRegistry,
        capabilities::{COMMANDS_UNIT, SSH_UNIT},
    },
    preferences::{ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId},
    sessions::UiSessionBinding,
    transfers::TransferOrigin,
};
use kerminal_domain::{
    files::FileEndpoint,
    identity::{HostId, TabId},
};

use crate::{
    features::commands::SnippetHostMoveRequested,
    features::operations::{OperationsEvent, OperationsSection, OperationsView},
    plugins::tools::{ProjectionTicket, ToolContext, ToolProjection, ToolScope},
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
        ToolRailLayout, ToolRailPanelSlots, WorkbenchToolPanelRenderers, WorkbenchToolPanels,
        attached_dock_default_width, focused_workspace_pane,
    },
    workspace_runtime::RuntimePanePhase,
};

/// 明确打开与快捷保存共享一次居中焦点来源；保存仍串行复用 Preferences 的 Ready/CAS 门禁。
pub(super) struct ToolRailPlacementFocusIntent {
    tool: ToolRailToolId,
    navigation_generation: u64,
    expected_focus: FocusHandle,
}

/// 迁移仅暂存原实体的焦点交接，任意新的导航或物理焦点变化都会使其失效。
struct SnippetHostFocusIntent {
    target: SnippetHostMoveRequested,
    commands: Entity<crate::features::commands::CommandsView>,
    search_focus: FocusHandle,
    expected_focus: FocusHandle,
    navigation_generation: u64,
    focus_generation: u64,
}

/// ToolRail 工具在目录归属之外还依赖的启停单元；UI 侧只在本文维护这一张映射。
/// 与 MCP `capabilities::tool_units` 和 `guides_tools` 保持一致：tmux、服务器信息、
/// 容器都经 SSH 会话执行，端口转发同时要求 ports 与 SSH；ports/containers 的自身
/// 单元已由注册表 owner 判定覆盖，这里只补跨族依赖，避免入口与真实执行能力对不上。
/// sftp 只按自身单元判定，目标协议是否可用由连接层拒绝；context、agentLauncher、
/// snippets、logs 维持目录 owner 的原有依赖，不在这里重复推断。
///
/// @author kongweiguang
fn tool_rail_required_units(tool: ToolRailToolId) -> &'static [&'static str] {
    match tool {
        ToolRailToolId::Tmux
        | ToolRailToolId::System
        | ToolRailToolId::Containers
        | ToolRailToolId::Ports => &[SSH_UNIT],
        _ => &[],
    }
}

/// 右栏入口、快捷键、程序化导航和命令面板统一读取冻结插件目录的启用视图。
/// ToolRail 身份到目录工具 ID 的映射只放在这里，避免各处各写一份判定后互相漂移；
/// 禁用只过滤入口，持久顺序与停靠仍由设置层保留，重新启用后按原偏好复原。
/// 空目录代表独立界面/测试未装配插件服务（生产组合根始终注入完整快照），
/// 此时沿用原有入口，避免把所有 Rail 工具误判为禁用。
///
/// @author kongweiguang
pub(super) fn tool_rail_tool_enabled(plugins: &PluginRegistry, tool: ToolRailToolId) -> bool {
    plugins.tools().is_empty()
        || (plugins.tool_enabled(tool.as_str())
            && tool_rail_required_units(tool)
                .iter()
                .all(|unit| plugins.is_enabled(unit)))
}

/// 命令面板需要一份禁用工具身份集合；复用与右栏完全相同的判定，
/// 使 SSH 依赖导致的隐藏（tmux、容器、端口、服务器信息）不会只发生在图标入口。
///
/// @author kongweiguang
pub(super) fn disabled_tool_rail_tools(plugins: &PluginRegistry) -> BTreeSet<ToolRailToolId> {
    plugins
        .tools()
        .iter()
        .filter_map(|tool| ToolRailToolId::from_id(tool.id.as_str()).ok())
        .filter(|tool| !tool_rail_tool_enabled(plugins, *tool))
        .collect()
}

/// 片段与工作流内容直接依赖 commands 单元；程序化入口不经过 Rail 图标，
/// 必须在改动中心页或宿主状态前用同一启用视图拒绝。空目录仍视为未装配插件服务。
///
/// @author kongweiguang
pub(super) fn commands_content_enabled(plugins: &PluginRegistry) -> bool {
    plugins.tools().is_empty() || plugins.is_enabled(COMMANDS_UNIT)
}

impl RootView {
    /// Root 侧调用点复用模块级判定；纯函数边界也让冻结视图可在不启动窗口时直接覆盖。
    ///
    /// @author kongweiguang
    pub(super) fn tool_rail_tool_enabled(&self, tool: ToolRailToolId) -> bool {
        tool_rail_tool_enabled(&self.services.plugins, tool)
    }

    /// 命令片段宿主与 Rail 图标共享同一条 commands 门禁，避免只有一处补校验。
    ///
    /// @author kongweiguang
    pub(super) fn commands_content_enabled(&self) -> bool {
        commands_content_enabled(&self.services.plugins)
    }

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
            .map(|(endpoint, _, _)| endpoint)
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
        Some(cx.new(|cx| {
            let mut panel = crate::features::files::FilesPane::new(
                files, transfers, executor, targets, endpoint,
            );
            panel.set_interface_density(
                kerminal_application::preferences::InterfaceDensity::Compact,
                cx,
            );
            panel
        }))
    }

    /// Root 子模块共用确认快照中的 Rail 投影；未载入时沿用默认，不建立另一份布局 owner。
    ///
    /// @author kongweiguang
    pub(super) fn current_tool_rail_settings(&self) -> ToolRailSettings {
        let mut settings = self
            .settings_snapshot
            .as_ref()
            .map(|snapshot| snapshot.tool_rail.clone())
            .unwrap_or_default();
        for tool in self.plugin_tools.registered_tools() {
            if !settings.order.contains(&tool) {
                settings.order.push(tool);
            }
        }
        settings
    }

    /// 作用域只使用已提交的活动标签和选中主机，不从异步视图猜测目标。
    ///
    /// @author kongweiguang
    pub(super) fn plugin_tool_context(&self, cx: &Context<Self>) -> ToolContext {
        let tab = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone());
        let binding = self
            .current_logs_target(cx)
            .map(|target| target.binding().clone());
        let selected_host = self
            .selected_host_id
            .clone()
            .or_else(|| self.selected_plugin_host_id.clone());
        ToolContext::new(tab, selected_host).with_active_terminal(binding)
    }

    /// 显式打开创建插件实例并重放一次挂载投影；失败保持旧布局，不随重绘自动重试。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_plugin_tool(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(tool, ToolRailToolId::Plugin(_)) {
            return true;
        }
        let context = self.plugin_tool_context(cx);
        match self.plugin_tools.ensure(tool, &context, window, cx) {
            Ok(()) => {
                self.plugin_tools.invalidate_projection();
                true
            }
            Err(error) => {
                self.status = Some(format!("工具 {} 无法打开：{error}", tool.as_str()).into());
                cx.notify();
                false
            }
        }
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
        // 点击、命令面板动作和程序化导航都汇入此入口；禁用工具在改动任何投影状态前退出，
        // 既不创建插件实例，也不改写持久布局。
        if !self.tool_rail_tool_enabled(tool) {
            return;
        }
        self.sync_tool_rail_active_tab();
        self.pending_tool_rail_placement_focus = None;
        // 明确切换到任意工具代表用户离开当前 Agent 首帧；迟到 Ready 不能再抢回输入焦点。
        self.pending_agent_focus_intent = None;
        let settings = self.current_tool_rail_settings();
        let before = self.tool_rail_state.project(&settings);

        if (!contains_tool(before.panels, tool)
            || (before.panels.attached == Some(tool) && self.right_dock_collapsed))
            && !self.prepare_plugin_tool(tool, window, cx)
        {
            return;
        }

        if tool == ToolRailToolId::Snippets
            && (!contains_tool(before.panels, tool)
                || (before.panels.attached == Some(tool) && self.right_dock_collapsed))
            && !self.return_workspace_before_snippets_rail(window, cx)
        {
            return;
        }

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
        // 隐藏原生验收只替代 OS 活动状态，仍保留生产路径全部模态与目标失效门禁。
        let window_active = window.is_window_active();
        #[cfg(feature = "right-rail-acceptance")]
        let window_active = window_active || self.right_rail_acceptance_active_override;
        !window_active
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

    /// 左、右、底部宿主只接收其子树冒泡的未消费 Escape；先复核槽位 owner 和折叠状态，避免迟到事件关闭替换面板。
    ///
    /// @author kongweiguang
    pub(super) fn handle_docked_tool_panel_key_down(
        &mut self,
        placement: ToolRailPanelPlacement,
        rendered_tool: ToolRailToolId,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        if let Some(tool) = docked_tool_panel_escape_target(
            &event.keystroke.key,
            rendered_tool,
            placement,
            panels,
            self.tool_rail_rendered_compact.0,
            self.right_dock_collapsed,
            self.tool_panel_overlay_focus_is_blocked(window, cx),
        ) {
            cx.stop_propagation();
            self.toggle_tool_rail_panel(tool, window, cx);
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

    /// 仅投影当前 Ready、精确 UI owner 的 pane 与完整 binding；有效 OSC7 目录优先于启动默认值。
    /// 目录只来自通过 session/revision 门禁的缓存，resize 清画面不使文件面板跳回旧目录。
    ///
    /// @author kongweiguang
    fn active_terminal_files_target(
        &self,
    ) -> Option<(FileEndpoint, Option<ExternalTargetLease>, UiSessionBinding)> {
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
        Some((endpoint, lease, binding.clone()))
    }

    /// 只有实际显示的活动文件可以提供浏览目录；后台文件与其它中央工具不能恢复已失效的目标。
    ///
    /// @author kongweiguang
    pub(super) fn active_file_browser_target(
        &self,
    ) -> Option<(FileEndpoint, Option<ExternalTargetLease>)> {
        if self.center_panel != CenterPanel::Editor {
            return None;
        }
        let tab_id = self
            .workspace_controller
            .as_ref()?
            .snapshot()
            .active_tab_id
            .as_ref()?;
        self.workspace_files.file_browser_target(tab_id)
    }

    /// 终端和文件工作区显式分流，文件内容激活不能变成目录跟随；两者都冻结各自真实来源。
    /// 文件入口只传递原文件系统及 lease，不从无 binding 推断隐式终端能力。
    ///
    /// @author kongweiguang
    pub(super) fn try_bind_files_from_active_terminal(&mut self, cx: &mut Context<Self>) {
        if !self.tab_tool_contents_are_current() {
            return;
        }
        let (endpoint, lease, binding, origin) = self
            .active_terminal_files_target()
            .map(|(endpoint, lease, binding)| {
                let origin = match &binding.owner {
                    SessionOwner::Ui { pane_id, .. } => {
                        Some(TransferOrigin::TerminalPane(pane_id.clone()))
                    }
                    _ => None,
                };
                (Some(endpoint), lease, Some(binding), origin)
            })
            .or_else(|| {
                let (endpoint, lease) = self.active_file_browser_target()?;
                let tab_id = self
                    .workspace_controller
                    .as_ref()?
                    .snapshot()
                    .active_tab_id
                    .clone()?;
                Some((
                    Some(endpoint),
                    lease,
                    None,
                    Some(TransferOrigin::WorkspaceFile(tab_id)),
                ))
            })
            .unwrap_or((None, None, None, None));
        let workspace_origins = match origin.as_ref() {
            Some(TransferOrigin::WorkspaceFile(id)) => {
                Some(self.workspace_files.host_transfer_origins(id))
            }
            _ => None,
        };
        if let Some(files) = self.sftp_panel.clone() {
            let right_browser_context = match origin.as_ref() {
                Some(TransferOrigin::WorkspaceFile(id)) => {
                    self.workspace_files.take_right_browser_context(id)
                }
                _ => None,
            };
            files.update(cx, move |files, cx| {
                files.set_transfer_origin(origin.clone(), cx);
                match (origin, endpoint) {
                    (Some(TransferOrigin::WorkspaceFile(workspace_id)), Some(endpoint)) => {
                        if let Some(snapshot) = right_browser_context
                            && !files.restore_file_browser_context(snapshot, cx)
                        {
                            // 已绑定、已加载、忙碌或快照无效时保留目标，不重试旧来源状态。
                            tracing::debug!("目标右栏已有状态或快照无效，丢弃本次浏览交接");
                        }
                        files.set_file_workspace_target(workspace_id, endpoint, lease, cx);
                        if let Some(origins) = workspace_origins {
                            files.set_transfer_query_scope(
                                kerminal_application::transfers::TransferQueryScope::origins(
                                    origins,
                                ),
                                cx,
                            );
                        }
                    }
                    (_, endpoint) => {
                        files.set_current_terminal_target(endpoint, lease, binding, cx)
                    }
                }
            });
        }
    }

    /// 配置 CAS 成功后按新设置投影并恢复目标工具的显式宽度，不把重排后的受限布局当用户意图。
    ///
    /// @author kongweiguang
    pub(super) fn remap_tool_rail_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_tool_rail_active_tab();
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
        self.reconcile_snippets_host(window, cx);
        cx.notify();
    }

    /// 将当前工具的明确拖动尺寸作为右栏共享偏好持久化，默认值回到统一尺寸时清除旧覆盖。
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
            130,
            1_360,
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
            for shared_tool in [
                ToolRailToolId::AgentLauncher,
                ToolRailToolId::Sftp,
                ToolRailToolId::Containers,
                ToolRailToolId::Snippets,
                ToolRailToolId::Tmux,
                ToolRailToolId::Ports,
                ToolRailToolId::System,
                ToolRailToolId::Logs,
            ] {
                layout.dock_panel_widths.insert(shared_tool, content);
            }
        } else {
            layout.dock_panel_widths.clear();
        }
        self.request_workbench_layout_intent(layout, cx);
        cx.notify();
    }

    /// 所有右栏工具共享一个默认宽度，Agent 运行态变化也不触发尺寸切换。
    ///
    /// @author kongweiguang
    pub(super) fn current_attached_default_width(&self, _tool: ToolRailToolId) -> gpui_kit::Pixels {
        attached_dock_default_width(self.tool_rail_state.viewport_width())
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

    /// 在事件边界恢复活动标签的工具顺序和折叠状态，避免下一帧之前误用旧标签的槽位。
    ///
    /// @author kongweiguang
    pub(super) fn sync_tool_rail_active_tab(&mut self) {
        let (active_tab_id, live_tab_ids) = self
            .workspace_controller
            .as_ref()
            .map(|controller| {
                let snapshot = controller.snapshot();
                (
                    snapshot.active_tab_id.clone(),
                    snapshot
                        .terminal_tabs
                        .iter()
                        .map(|tab| tab.id().clone())
                        .collect::<Vec<TabId>>(),
                )
            })
            .unwrap_or_default();
        if let Some(collapsed) = self.tool_rail_state.activate_tab(
            active_tab_id,
            &live_tab_ids,
            self.right_dock_collapsed,
        ) {
            self.right_dock_collapsed = collapsed;
        }
    }

    /// 事件边界与渲染共享同一份 Rail 布局和上下文，避免右侧面板在首个绘制帧继续用上一 pane 的目标。
    /// 这里只计算宿主可见项，不创建实例、不执行插件回调；渲染阶段仍复用同一结果并保留原有 defer 兜底。
    ///
    /// @author kongweiguang
    fn project_tool_rail(&mut self, cx: &Context<Self>) -> (ToolRailLayout, ToolContext) {
        let settings = self.current_tool_rail_settings();
        self.sync_tool_rail_active_tab();
        let previous_tool = self.tool_rail_state.attached_tool();
        let mut layout = self.tool_rail_state.project(&settings);
        let plugin_context = self.plugin_tool_context(cx);
        layout.entries.main.retain(|tool| {
            self.plugin_tools.is_applicable(*tool, &plugin_context)
                && self.tool_rail_tool_enabled(*tool)
        });
        layout.entries.bottom.retain(|tool| {
            self.plugin_tools.is_applicable(*tool, &plugin_context)
                && self.tool_rail_tool_enabled(*tool)
        });
        for slot in [
            &mut layout.panels.attached,
            &mut layout.panels.left,
            &mut layout.panels.bottom,
            &mut layout.panels.center,
        ] {
            // 槽位投影来自持久偏好，禁用时只清空本帧的宿主槽位；偏好本身保持原值，
            // 重新启用后同一 order/placement 会再次生效。
            if slot.is_some_and(|tool| {
                !self.plugin_tools.is_applicable(tool, &plugin_context)
                    || !self.tool_rail_tool_enabled(tool)
            }) {
                *slot = None;
            }
        }
        if previous_tool != layout.panels.attached {
            let next_default = layout
                .panels
                .attached
                .map(|tool| self.current_attached_default_width(tool));
            self.tool_rail_state
                .set_attached_tool(layout.panels.attached, next_default);
            self.prepare_attached_dock_width_restore(layout.panels.attached);
        }
        (layout, plugin_context)
    }

    /// 只按已提交布局和上下文推算可见工具，供事件边界与渲染共用，避免两处形成不同的挂载集合。
    /// `window_active` 在隐藏窗口也按真实状态投影，显示时仍由窗口激活事件重新提交。
    ///
    /// @author kongweiguang
    fn projection_for_layout(
        &mut self,
        layout: &ToolRailLayout,
        plugin_context: &ToolContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolProjection {
        let window_active = window.is_window_active();
        #[cfg(feature = "right-rail-acceptance")]
        let window_active = window_active || self.right_rail_acceptance_active_override;

        let visible = [
            layout.panels.attached,
            layout.panels.left,
            layout.panels.bottom,
            layout.panels.center,
        ]
        .into_iter()
        .flatten()
        .filter(|tool| {
            self.plugin_tools.is_applicable(*tool, plugin_context)
                && self.tool_rail_tool_enabled(*tool)
                && super::workbench::tool_rail_panel_mounted(self, layout.panels, *tool, window, cx)
        })
        .collect();
        ToolProjection {
            context: plugin_context.clone(),
            visible,
            window_active,
        }
    }

    /// 真实焦点或标签激活后，在事件安全阶段换挂当前 Tab 的既有实体并提交完整工具投影，
    /// 使新 Logs/Operations/SFTP 目标与插件 context/visibility 早于窗口首个绘制帧生效。
    /// 插件实例创建和上下文回调只在此事件阶段执行，渲染仍只读取已提交投影；投影换代会让旧 defer 失效。
    /// 先换挂 Agent/工具实体再计算投影，保留 Tab 级草稿和绑定，不在过渡期把新目标写进旧实体。
    ///
    /// @author kongweiguang
    pub(super) fn sync_tool_projection_before_first_draw(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reconcile_agent_panel_for_tab(window, cx);
        self.reconcile_tab_tool_contents(window, cx);
        if !self.tab_tool_contents_are_current() {
            return;
        }
        let (layout, plugin_context) = self.project_tool_rail(cx);
        let projection = self.projection_for_layout(&layout, &plugin_context, window, cx);
        let ticket = self.plugin_tools.stage_projection(projection.clone());
        self.apply_tool_projection(ticket, projection, window, cx);
    }

    /// 工具栏读取当前标签投影；宿主迁移必须已由导航或布局事件完成。
    /// 扩展创建与生命周期只随投影变化或显式打开执行，同一投影的重绘仅接收内置视图更新。
    ///
    /// @author kongweiguang
    pub(super) fn workbench_tool_panels(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WorkbenchToolPanels {
        let (layout, plugin_context) = self.project_tool_rail(cx);
        let theme = ThemeTokens::current(window, cx);
        let projection = self.projection_for_layout(&layout, &plugin_context, window, cx);
        let window_active = projection.window_active;
        let ticket = self.plugin_tools.stage_projection(projection.clone());
        // 值相同也必须提交：render 可能刚暂存同一投影而生命周期还没执行，或内置活动需要更新。
        cx.defer_in(window, move |root, window, cx| {
            root.apply_tool_projection(ticket, projection, window, cx);
        });

        let mut renderers = WorkbenchToolPanelRenderers::default();
        for tool in self.plugin_tools.registered_tools() {
            let renderer = self
                .plugin_tools
                .view_for_mount(tool, &plugin_context, window_active)
                .map_or_else(
                    || {
                        if matches!(tool, ToolRailToolId::Plugin(_)) {
                            self.render_plugin_tool_recovery(tool, theme, cx)
                        } else {
                            tool_unavailable_renderer(tool, theme)
                        }
                    },
                    |view| view.into_any_element(),
                );
            renderers.insert(tool, renderer);
        }

        WorkbenchToolPanels { layout, renderers }
    }

    /// 暂时不可用的扩展工具在原位置显式重试，普通重绘不申请资源，也不改变停靠布局。
    /// 关闭中或等待关闭重试的 owner 由原关闭链处理，不在这里重新创建。
    fn render_plugin_tool_recovery(
        &self,
        tool: ToolRailToolId,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let context = self.plugin_tool_context(cx);
        let can_retry = matches!(
            self.plugin_tools.phase(tool, &context),
            None | Some(crate::plugins::tools::ToolPhase::Open)
        );
        div()
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .text_color(theme.muted)
            .child(crate::locale::text(
                "工具暂时不可用。",
                "The tool is temporarily unavailable.",
            ))
            .children(can_retry.then(|| {
                gpui_kit::component::button::Button::new(gpui_kit::SharedString::from(format!(
                    "retry-plugin-tool-{}",
                    tool.as_str()
                )))
                .label(crate::locale::text("重试", "Retry"))
                .on_click(cx.listener(move |root, _, window, cx| {
                    if !root.host_shutdown_requested
                        && !root.tool_panel_overlay_focus_is_blocked(window, cx)
                    {
                        if root.prepare_plugin_tool(tool, window, cx) {
                            // 重试成功后立即在点击事件内提交新代投影，避免先画一帧恢复占位。
                            root.sync_tool_projection_before_first_draw(window, cx);
                        }
                        cx.notify();
                    }
                }))
            }))
            .into_any_element()
    }

    /// 只应用仍然有效的挂载票据；同一代只发起一次插件生命周期，旧代次即使投影值相同也拒绝。
    /// 内置面板活动和目标刷新每帧仍执行，避免把“值未变”误当成宿主状态无需更新。
    ///
    /// @author kongweiguang
    fn apply_tool_projection(
        &mut self,
        ticket: ProjectionTicket,
        projection: ToolProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.plugin_tools.projection_is_current(ticket) {
            return;
        }
        let needs_attempt = self.plugin_tools.projection_lifecycle_needs_attempt(ticket);
        let mut did_attempt = false;
        if needs_attempt {
            // 先标记再回调：插件回调若同步触发重入，同一代不会重复 ensure 或显隐通知。
            self.plugin_tools
                .mark_projection_lifecycle_attempted(ticket);
            did_attempt = true;
            for (tool, section) in [
                (ToolRailToolId::Containers, OperationsSection::Containers),
                (ToolRailToolId::Tmux, OperationsSection::Tmux),
                (ToolRailToolId::Ports, OperationsSection::PortForwards),
                (ToolRailToolId::System, OperationsSection::ServerInfo),
            ] {
                if projection.visible.contains(&tool) {
                    let _ = self.ensure_operations_panel(tool, section, window, cx);
                }
            }
            if projection.visible.contains(&ToolRailToolId::Sftp) {
                self.ensure_sftp_panel(window, cx);
                self.try_bind_files_from_active_terminal(cx);
            }
        }
        let previous_mounts = projection
            .visible
            .iter()
            .map(|tool| {
                self.plugin_tools
                    .view_for_mount(*tool, &projection.context, projection.window_active)
                    .map(|view| view.entity_id())
            })
            .collect::<Vec<_>>();
        let changed = self.refresh_builtin_tool_instances(&projection, window, cx);
        if did_attempt {
            for tool in &projection.visible {
                if matches!(tool, ToolRailToolId::Plugin(_))
                    && let Err(error) =
                        self.plugin_tools
                            .ensure(*tool, &projection.context, window, cx)
                {
                    self.status = Some(format!("工具 {} 无法打开：{error}", tool.as_str()).into());
                }
            }
        }
        self.refresh_operations_panel_targets(cx);
        self.sync_shared_panel_activity(&projection.visible, projection.window_active, window, cx);
        if did_attempt {
            // 回调可能同步改变投影；旧代不得再请求可见性或 Agent 快照。
            if !self.plugin_tools.projection_is_current(ticket) {
                return;
            }
            match self.plugin_tools.sync_visibility(
                &projection.context,
                &projection.visible,
                projection.window_active,
                cx,
            ) {
                Ok(()) => {
                    // 恢复成功后撤掉旧失败提示，避免用户继续按过期状态排查。
                    if self
                        .status
                        .as_ref()
                        .is_some_and(|status| status.to_string().contains("状态未确认"))
                    {
                        self.status = None;
                    }
                }
                Err(error) => {
                    // 隐藏失败不会制造可见的宿主面板，必须用状态提示保留可恢复入口；
                    // 同代去重仍保留下一次显式重开或新投影的重新确认机会。
                    let unconfirmed = self
                        .plugin_tools
                        .unconfirmed_tools()
                        .into_iter()
                        .map(|tool| tool.as_str().to_owned())
                        .collect::<Vec<_>>()
                        .join("、");
                    let subject = if unconfirmed.is_empty() {
                        "工具".to_owned()
                    } else {
                        format!("工具 {unconfirmed}")
                    };
                    self.status = Some(
                        format!("{subject}状态未确认：{error}；重新打开对应工具可重试").into(),
                    );
                }
            }
            if self
                .plugin_tools
                .projected_tool_active(ToolRailToolId::AgentLauncher)
            {
                self.request_agent_snapshot(cx);
            }
        }
        let mounts_changed = projection
            .visible
            .iter()
            .map(|tool| {
                self.plugin_tools
                    .view_for_mount(*tool, &projection.context, projection.window_active)
                    .map(|view| view.entity_id())
            })
            .ne(previous_mounts);
        if did_attempt || changed || mounts_changed {
            cx.notify();
        }
    }

    /// 工厂只借当前标签的已保留实体；切换尚未提交时不把旧标签句柄绑定到新标签。
    /// Agent 服务可独立缺席，只有 Agent 视图需要核对自身标签，其他工具仍按工作区标签投影。
    ///
    /// @author kongweiguang
    fn refresh_builtin_tool_instances(
        &mut self,
        projection: &ToolProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.tab_tool_contents_are_current()
            || self.tab_tool_contents_tab_id.as_ref() != projection.context.active_tab()
        {
            return false;
        }
        let operation = |tool| {
            self.tool_rail_operations_views
                .get(&tool)
                .cloned()
                .map(AnyView::from)
        };
        let entries: [(ToolRailToolId, ToolScope, Option<AnyView>); 8] = [
            (
                ToolRailToolId::AgentLauncher,
                ToolScope::Tab,
                self.agents_panel.clone().map(AnyView::from),
            ),
            (
                ToolRailToolId::Sftp,
                ToolScope::Tab,
                self.sftp_panel.clone().map(AnyView::from),
            ),
            (
                ToolRailToolId::Snippets,
                ToolScope::Tab,
                self.commands_view.clone().map(AnyView::from),
            ),
            (
                ToolRailToolId::Tmux,
                ToolScope::Tab,
                operation(ToolRailToolId::Tmux),
            ),
            (
                ToolRailToolId::Ports,
                ToolScope::Tab,
                operation(ToolRailToolId::Ports),
            ),
            (
                ToolRailToolId::System,
                ToolScope::Tab,
                operation(ToolRailToolId::System),
            ),
            (
                ToolRailToolId::Logs,
                ToolScope::Tab,
                self.logs_view.clone().map(AnyView::from),
            ),
            (
                ToolRailToolId::Containers,
                ToolScope::Window,
                operation(ToolRailToolId::Containers),
            ),
        ];
        let mut changed = false;
        for (tool, scope, view) in entries {
            if tool == ToolRailToolId::AgentLauncher
                && self.agent_panel_tab_id.as_ref() != projection.context.active_tab()
            {
                continue;
            }
            match self
                .plugin_tools
                .set_builtin_view(tool, scope, view, &projection.context)
            {
                Ok(updated) => changed |= updated,
                Err(error) => {
                    self.status = Some(format!("工具 {} 无法装配：{error}", tool.as_str()).into())
                }
            }
            if projection.visible.contains(&tool)
                && self.plugin_tools.phase(tool, &projection.context).is_none()
            {
                match self
                    .plugin_tools
                    .ensure(tool, &projection.context, window, cx)
                {
                    Ok(()) => changed = true,
                    Err(error) => {
                        self.status =
                            Some(format!("工具 {} 无法打开：{error}", tool.as_str()).into())
                    }
                }
            }
        }
        if changed
            && let Err(error) = self.plugin_tools.sync_visibility(
                &projection.context,
                &projection.visible,
                projection.window_active,
                cx,
            )
        {
            self.status = Some(format!("工具状态同步失败：{error}").into());
        }
        changed
    }

    /// SFTP 首次出现时先固定为紧凑面板，避免首帧沿用共享工作台的来源和布局。
    ///
    /// @author kongweiguang
    fn ensure_sftp_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sftp_panel.is_some() {
            return;
        }
        let panel = Self::new_files_panel(&self.services, cx);
        if let Some(panel) = panel.as_ref() {
            let density = super::preferences::current_interface_density(self);
            panel.update(cx, |panel, cx| {
                panel.set_compact_layout(true, cx);
                panel.set_interface_density(density, cx);
            });
        }
        let subscriptions = panel
            .as_ref()
            .map(|panel| Self::subscribe_files_pane_events(panel, window, cx));
        self.sftp_panel = panel;
        self._sftp_panel_subscriptions = subscriptions;
    }

    /// 导航前重新投影真实宿主，折叠和窄窗抽屉不会因旧槽位被误认为已经显示。
    ///
    /// @author kongweiguang
    pub(super) fn snippets_mounted_in_tool_rail(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        super::workbench::tool_rail_panel_mounted(
            self,
            panels,
            ToolRailToolId::Snippets,
            window,
            cx,
        )
    }

    /// 中央宿主持有片段时关闭当前标签的工具槽，防止响应式布局重新挂载同一实体。
    ///
    /// @author kongweiguang
    pub(super) fn release_snippets_tool_rail(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_tool_rail_active_tab();
        let settings = self.current_tool_rail_settings();
        let before = self.tool_rail_state.project(&settings).panels;
        if !self
            .tool_rail_state
            .close(&settings, ToolRailToolId::Snippets)
        {
            return;
        }
        let after = self.tool_rail_state.project(&settings).panels;
        if before.attached != after.attached {
            let next_default = after
                .attached
                .map(|tool| self.current_attached_default_width(tool));
            self.tool_rail_state
                .set_attached_tool(after.attached, next_default);
            self.prepare_attached_dock_width_restore(after.attached);
        }
        if self.compact_tool_drawer_open
            && self.tool_rail_rendered_compact.1 == Some(ToolRailToolId::Snippets)
        {
            self.close_compact_drawer_for_center_navigation(window, cx);
        }
        cx.notify();
    }

    /// 设置或窗口变化在绘制前核对宿主；模态拥有焦点时只更新底层路由。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_snippets_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_tool_rail_active_tab();
        if self.center_panel != CenterPanel::Commands || self.commands_view.is_none() {
            return;
        }
        let settings = self.current_tool_rail_settings();
        let panels = self.tool_rail_state.project(&settings).panels;
        if !contains_tool(panels, ToolRailToolId::Snippets) {
            return;
        }
        if !self.snippet_editor_modal_open
            && super::workbench::tool_rail_panel_mounted(
                self,
                panels,
                ToolRailToolId::Snippets,
                window,
                cx,
            )
        {
            self.restore_workspace_for_snippets_rail(window, cx);
        } else {
            self.release_snippets_tool_rail(window, cx);
        }
    }

    /// 显式迁移片段时恢复当前标签所属的真实内容，避免同一实体同时占据中央和侧栏。
    ///
    /// @author kongweiguang
    pub(super) fn return_workspace_before_snippets_rail(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.center_panel != CenterPanel::Commands {
            return true;
        }
        if self.preferences_modal_open || self.connections_modal_open {
            return false;
        }
        self.restore_workspace_for_snippets_rail(window, cx);
        self.center_panel != CenterPanel::Commands
    }

    /// 恢复底层标签时不请求焦点；空工作区也不能抢走设置或连接模态的输入。
    ///
    /// @author kongweiguang
    fn restore_workspace_for_snippets_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_tab = self.workspace_controller.as_ref().and_then(|controller| {
            let snapshot = controller.snapshot();
            snapshot.active_tab_id.as_ref().and_then(|id| {
                snapshot
                    .terminal_tabs
                    .iter()
                    .any(|tab| tab.id() == id)
                    .then(|| id.clone())
            })
        });
        if let Some(tab_id) = active_tab {
            self.activate_workspace_tab_surface(&tab_id, false, window, cx);
        } else {
            self.center_panel = CenterPanel::Terminal;
            self.clear_pending_pane_focus();
            self.clear_keybinding_chord();
            self.pending_key_commits.clear();
            self.focused_agent_terminal = false;
            self.set_reported_focus_session(None, cx);
            self.refresh_commands_session(cx);
            self.refresh_workspace_views(cx);
            self.request_visible_workspace_snapshots(cx);
            self.refresh_proxy_panel(cx);
            self.refresh_active_suggestion_context(cx);
            self.cancel_unfocused_pane_compositions(window, cx);
            cx.notify();
        }
    }

    /// 按实际源宿主迁移当前标签的实体，避免迟到点击或上层模态改变其他页面。
    /// 工具栏使用现有投影和抽屉入口，中央使用既有路由，随后共用普通导航的焦点交接。
    ///
    /// @author kongweiguang
    pub(super) fn move_snippets_host(
        &mut self,
        target: SnippetHostMoveRequested,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        use kerminal_application::AppError;

        if !self.tab_tool_contents_are_current() {
            return Err(AppError::conflict("当前标签的片段视图尚未就绪"));
        }
        if self.commands_view.is_none() {
            return Err(AppError::not_found("命令片段服务尚未配置"));
        }
        if self.tool_panel_overlay_focus_is_blocked(window, cx) {
            return Err(AppError::conflict("请先完成当前操作再移动片段"));
        }
        self.sync_tool_rail_active_tab();
        match target {
            SnippetHostMoveRequested::ToolRail => {
                if self.center_panel != CenterPanel::Commands {
                    return Err(AppError::conflict("片段已不在工作区"));
                }
                if !self.return_workspace_before_snippets_rail(window, cx) {
                    return Err(AppError::conflict("无法恢复当前工作区"));
                }
                self.ensure_tool_panel_visible(ToolRailToolId::Snippets, window, cx);
                if self.tool_rail_rendered_compact.0 && !self.compact_tool_drawer_open {
                    self.show_compact_tool_panel(ToolRailToolId::Snippets, window, cx);
                }
                if self.center_panel == CenterPanel::Commands
                    || !self.snippets_mounted_in_tool_rail(window, cx)
                {
                    return Err(AppError::conflict("片段工具未能移动到工具栏"));
                }
            }
            SnippetHostMoveRequested::Center => {
                if self.center_panel == CenterPanel::Commands
                    || !self.snippets_mounted_in_tool_rail(window, cx)
                {
                    return Err(AppError::conflict("片段已不在工具栏"));
                }
                self.set_center_panel(CenterPanel::Commands, window, cx);
                if self.center_panel != CenterPanel::Commands {
                    return Err(AppError::conflict("片段页面未能移动到工作区"));
                }
            }
        }
        self.open_snippets_content(window, cx);
        let Some(commands) = self.commands_view.clone() else {
            return Err(AppError::not_found("命令片段服务尚未配置"));
        };
        if commands.update(cx, |commands, cx| commands.focus_snippets(window, cx))
            && let Some(search_focus) = window.focused(cx)
        {
            self.pending_tool_rail_placement_focus = None;
            self.clear_pending_pane_focus();
            let expected_focus =
                if target == SnippetHostMoveRequested::ToolRail && self.compact_tool_drawer_open {
                    self.compact_drawer_focus_handle.clone()
                } else {
                    self.focus_handle.clone()
                };
            window.focus(&expected_focus, cx);
            let intent = SnippetHostFocusIntent {
                target,
                commands,
                search_focus,
                expected_focus,
                navigation_generation: self.surface_navigation_generation,
                focus_generation: self.pane_focus_intent_generation,
            };
            // 首帧仍可能是旧宿主；下一帧之后才检查实际绘制树和延后装配的工具实体。
            cx.on_next_frame(window, move |root, window, cx| {
                root.complete_snippets_host_focus(intent, 4, false, window, cx);
            });
        }
        Ok(())
    }

    /// 等待目标实际进入焦点树；限定帧数并逐帧复核来源，防止迟到的装配覆盖新操作。
    ///
    /// @author kongweiguang
    fn complete_snippets_host_focus(
        &mut self,
        intent: SnippetHostFocusIntent,
        remaining_frames: u8,
        rendered_new_host: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.window_handle() != self.window_handle
            || self.surface_navigation_generation != intent.navigation_generation
            || self.pane_focus_intent_generation != intent.focus_generation
            || window.focused(cx).as_ref() != Some(&intent.expected_focus)
            || self.commands_view.as_ref() != Some(&intent.commands)
            || !self.tab_tool_contents_are_current()
            || self.tool_panel_overlay_focus_is_blocked(window, cx)
            || window.has_active_dialog(cx)
        {
            return;
        }
        let target_is_current = match intent.target {
            SnippetHostMoveRequested::Center => self.center_panel == CenterPanel::Commands,
            SnippetHostMoveRequested::ToolRail => {
                self.center_panel != CenterPanel::Commands
                    && self.snippets_mounted_in_tool_rail(window, cx)
            }
        };
        if !target_is_current {
            return;
        }
        if rendered_new_host && self.focus_handle.contains(&intent.search_focus, window) {
            window.focus(&intent.search_focus, cx);
        } else if remaining_frames > 0 {
            cx.on_next_frame(window, move |root, window, cx| {
                root.complete_snippets_host_focus(intent, remaining_frames - 1, true, window, cx);
            });
        }
    }

    /// 普通片段导航优先复用实际宿主，避免同一命令实体同时进入中央和工具栏。
    ///
    /// @author kongweiguang
    pub(super) fn open_snippets_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 终端建议、命令面板和快捷键都汇入这里；commands 禁用时在改动宿主或中心页前拒绝，
        // 保持原工作区原样，避免禁用插件仍被程序化导航拉到前台。
        if !self.commands_content_enabled() {
            return;
        }
        self.sync_tool_rail_active_tab();
        self.reconcile_tab_tool_contents(window, cx);
        self.reconcile_snippets_host(window, cx);
        let Some(commands) = self.commands_view.clone() else {
            self.status = Some(crate::locale::text(
                "命令片段服务尚未装配。",
                "The snippet editor is unavailable.",
            ));
            cx.notify();
            return;
        };
        let focus_blocked = self.tool_panel_overlay_focus_is_blocked(window, cx);
        if self.center_panel == CenterPanel::Commands {
            commands.update(cx, |commands, cx| {
                commands.select_snippets(cx);
                if !focus_blocked {
                    commands.focus_snippets(window, cx);
                }
            });
            return;
        }
        let mounted = self.snippets_mounted_in_tool_rail(window, cx);
        if !mounted {
            let settings = self.current_tool_rail_settings();
            let panels = self.tool_rail_state.project(&settings).panels;
            if contains_tool(panels, ToolRailToolId::Snippets) {
                self.ensure_tool_panel_visible(ToolRailToolId::Snippets, window, cx);
            } else {
                self.set_center_panel(CenterPanel::Commands, window, cx);
            }
        }
        self.cancel_visible_transfer_preparation(cx);
        self.cancel_file_sftp_selections(cx);
        self.cancel_plugin_host_open();
        self.surface_navigation_generation = self.surface_navigation_generation.wrapping_add(1);
        commands.update(cx, |commands, cx| {
            commands.select_snippets(cx);
            if !focus_blocked {
                commands.focus_snippets(window, cx);
            }
        });
        cx.notify();
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

    /// 按需建立当前标签的工具视图；容器和 Ports 从真实 pane 初始化，确认事件只更新当前实体。
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
        let (initial_container_context, initial_ports_container_target) =
            if tool == ToolRailToolId::Containers || tool == ToolRailToolId::Ports {
                let (context, ports_target) = self.current_container_panel_projection();
                (
                    Some(context),
                    (tool == ToolRailToolId::Ports)
                        .then_some(ports_target)
                        .flatten(),
                )
            } else {
                (None, None)
            };
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
            let view = match (
                tool,
                initial_container_context,
                initial_ports_container_target,
            ) {
                (ToolRailToolId::Containers, Some(context), _) => {
                    view.with_initial_container_panel_context(context)
                }
                (ToolRailToolId::Ports, _, Some(target)) => {
                    view.with_initial_ports_container_target(initial_tool_target, target)
                }
                _ => view.with_initial_tool_rail_target(initial_tool_target),
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
                let is_current = root
                    .tool_rail_operations_views
                    .values()
                    .any(|current| current.entity_id() == source.entity_id());
                if !is_current {
                    return;
                }
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

    /// 容器使用精确 session 上下文，Ports 只借它提示容器目标且阻止宿主回退；侧栏选择不能改写目标。
    /// 同源名称、副标题和 tmux 当前会话只作展示/快捷操作上下文，不覆盖旧标签工具的绑定与草稿。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_operations_panel_targets(&mut self, cx: &mut Context<Self>) {
        let tools_current = self.tab_tool_contents_are_current();
        let tmux_binding = super::operations::focused_tmux_quickref_binding(self);
        let tmux_attach_binding = super::operations::focused_tmux_attach_binding(self);
        let tmux_current_session_id = super::operations::focused_tmux_current_session_id(self);
        let tmux_current_session_name = super::operations::focused_tmux_current_session_name(self);
        let fixed_target = self.tool_rail_operation_target();
        let (container_context, ports_container_target) = self.current_container_panel_projection();
        let host_display_names = self
            .host_navigation
            .as_ref()
            .map(|navigation| navigation.read(cx).display_names())
            .unwrap_or_default();
        let host_subtitles = self
            .host_navigation
            .as_ref()
            .map(|navigation| navigation.read(cx).endpoint_subtitles())
            .unwrap_or_default();
        if let Some(view) = self.logs_view.clone() {
            view.update(cx, |view, cx| {
                view.set_host_display_names(host_display_names.clone(), cx);
            });
        }
        let views = self
            .tool_rail_operations_views
            .iter()
            .filter(|_| tools_current)
            .map(|(tool, view)| (view.clone(), Some(fixed_target.clone()), Some(*tool)))
            .chain(
                self.operations_view
                    .iter()
                    .cloned()
                    .map(|view| (view, None, None)),
            )
            .collect::<Vec<_>>();
        let selected_host_id = self.selected_host_id.clone();
        for (view, fixed_target, tool) in views {
            view.update(cx, |view, cx| {
                view.set_host_display_names(host_display_names.clone(), cx);
                view.set_host_subtitles(host_subtitles.clone(), cx);
                if tool == Some(ToolRailToolId::Containers) {
                    view.set_container_panel_context(container_context.clone(), cx);
                } else if let Some(target) = fixed_target {
                    if tool == Some(ToolRailToolId::Ports) {
                        view.set_tool_rail_target_with_container_target(
                            target,
                            ports_container_target.clone(),
                            cx,
                        );
                    } else {
                        view.set_tool_rail_target(target, cx);
                    }
                } else {
                    view.set_host_target(selected_host_id.clone(), cx);
                }
                view.set_tmux_quickref_binding(tmux_binding.clone(), cx);
                view.set_tmux_attach_binding(tmux_attach_binding.clone(), cx);
                view.set_tmux_current_session_id(tmux_current_session_id.clone(), cx);
                view.set_tmux_current_session_name(tmux_current_session_name.clone(), cx);
            });
        }
    }

    /// 同步右栏专属面板的活动状态；它们在四种停靠宿主中都保持面板布局，中心运维工作区仍独立使用完整布局。
    ///
    /// @author kongweiguang
    fn sync_shared_panel_activity(
        &mut self,
        visible_tools: &[ToolRailToolId],
        window_active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(files_pane) = self.sftp_panel.clone() {
            let sftp_active = window_active && visible_tools.contains(&ToolRailToolId::Sftp);
            files_pane.update(cx, |files_pane, cx| {
                files_pane.set_compact_layout(true, cx);
                files_pane.set_active(sftp_active, cx);
            });
        }
        if !self.snippet_editor_modal_open
            && let Some(commands_view) = self.commands_view.clone()
        {
            let compact = visible_tools.contains(&ToolRailToolId::Snippets);
            commands_view.update(cx, |commands_view, cx| {
                commands_view.set_compact_layout(compact, window, cx);
            });
        }
        let operation_views = self
            .tool_rail_operations_views
            .iter()
            .map(|(tool, view)| (*tool, view.clone()))
            .collect::<Vec<_>>();
        for (tool, view) in operation_views {
            let active = window_active && visible_tools.contains(&tool);
            view.update(cx, |view, cx| {
                view.set_compact_layout(true, cx);
                view.set_active(active, cx);
            });
        }

        let context_active = window_active && visible_tools.contains(&ToolRailToolId::Context);
        if let Some(panel) = self.context_panel.clone() {
            panel.update(cx, |panel, cx| panel.set_active(context_active, cx));
        }

        let logs_active = window_active && visible_tools.contains(&ToolRailToolId::Logs);
        let current_target = self.current_logs_target(cx).map(|target| {
            let Some(controller) = self.workspace_controller.as_ref() else {
                return target;
            };
            let Some(pane_id) = controller.snapshot().focused_pane_id.as_ref() else {
                return target;
            };
            if self.workspace_runtime.pane(pane_id).is_none() {
                return target;
            }
            target.with_pane_detail(pane_id, self.workspace_pane_title(pane_id, cx).to_string())
        });
        let snippet_command_source = current_target.as_ref().and_then(|target| {
            let binding = target.binding();
            let tools = self.workspace_tools.get(&binding.session_id)?;
            let block = tools
                .read(cx)
                .latest_submitted_command_block(&binding.session_id, cx)?;
            Some(crate::features::commands::SnippetCommandBlockSource {
                binding: binding.clone(),
                block_id: block.id,
                session_generation: block.session_generation,
                capture_revision: block.capture_revision,
                command: block.command,
            })
        });
        let theme_mode = self.theme_mode;
        // 测试构建记录投影目标；cfg! 常量分支让生产代码保持普通语句且不读取该字段。
        if cfg!(test) {
            self.projected_logs_target_for_test = current_target.clone();
        }
        if let Some(view) = self.logs_view.clone() {
            view.update(cx, |view, cx| {
                view.set_target(current_target.clone(), cx);
                view.set_theme_mode(theme_mode, cx);
                view.set_active(logs_active, cx);
            });
        }
        if let Some(view) = self.commands_view.clone() {
            let configuration_actions = self.services.commands_configuration_actions();
            view.update(cx, |view, cx| {
                view.set_snippet_history_target(current_target, cx);
                view.set_snippet_command_source(snippet_command_source, cx);
                view.set_configuration_actions(configuration_actions, cx);
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
            && self
                .plugin_tools
                .projected_tool_visible(ToolRailToolId::AgentLauncher)
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

/// Docked Escape 必须来自可见的原方向宿主；容器折叠、compact 转换、菜单/模态及替换工具都不接受旧回调。
///
/// @author kongweiguang
fn docked_tool_panel_escape_target(
    key: &str,
    rendered_tool: ToolRailToolId,
    placement: ToolRailPanelPlacement,
    panels: ToolRailPanelSlots,
    compact_panels: bool,
    attached_dock_collapsed: bool,
    focus_blocked: bool,
) -> Option<ToolRailToolId> {
    let current_owner = match placement {
        ToolRailPanelPlacement::Attached => panels.attached,
        ToolRailPanelPlacement::Left => panels.left,
        ToolRailPanelPlacement::Bottom => panels.bottom,
        ToolRailPanelPlacement::Center => panels.center,
    };
    let placement_is_docked = matches!(
        placement,
        ToolRailPanelPlacement::Attached
            | ToolRailPanelPlacement::Left
            | ToolRailPanelPlacement::Bottom
    );
    (key.eq_ignore_ascii_case("escape")
        && placement_is_docked
        && !compact_panels
        && !(placement == ToolRailPanelPlacement::Attached && attached_dock_collapsed)
        && current_owner == Some(rendered_tool)
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

/// 工具缺少真实实例时保留原有诊断标识；视图选择始终由注册工厂负责。
///
/// @author kongweiguang
fn tool_unavailable_renderer(tool: ToolRailToolId, theme: ThemeTokens) -> AnyElement {
    let (id, zh, en) = match tool {
        ToolRailToolId::AgentLauncher => (
            "tool-agent-launcher-unavailable",
            "Agent 服务尚未装配。",
            "The Agent service is unavailable.",
        ),
        ToolRailToolId::Sftp => (
            "tool-sftp-unavailable",
            "文件与传输服务尚未装配。",
            "File and transfer services are unavailable.",
        ),
        ToolRailToolId::Snippets => (
            "tool-snippets-unavailable",
            "命令服务尚未装配。",
            "Command services are unavailable.",
        ),
        ToolRailToolId::Logs => (
            "tool-logs-unavailable",
            "命令历史服务尚未装配。",
            "Command history is unavailable.",
        ),
        ToolRailToolId::Containers
        | ToolRailToolId::Tmux
        | ToolRailToolId::Ports
        | ToolRailToolId::System => (
            operations_unavailable_id(tool),
            "运维服务尚未装配。",
            "Operations services are unavailable.",
        ),
        _ => (
            "tool-plugin-unavailable",
            "工具尚未就绪，请重新打开。",
            "The tool is unavailable. Try opening it again.",
        ),
    };
    unavailable_panel(id, zh, en, theme)
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
