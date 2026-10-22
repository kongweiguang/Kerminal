//! Root 对 Quick Open 与命令面板的精确导航和输入提交协调。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::{
    preferences::ToolRailToolId,
    sessions::{
        TerminalCommandAvailability, UiInputProjection, UiSessionBinding, UiTerminalInputRequest,
    },
    workspace::WorkspaceSessionTab,
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::input::TerminalInputEvent;

use crate::features::quick_open::{
    QuickOpenEvent, QuickOpenMode, QuickOpenOpenContext, QuickOpenSelection,
    QuickOpenTerminalTarget,
};

use super::{CenterPanel, RootView, SessionCommand, terminal_keyboard};

impl RootView {
    /// 打开时固定 workspace 快照和当前可信 prompt binding，并镜像模态状态以隔离底层输入。
    /// 输入门禁只读 Root 的镜像，不能在 Command 回调持有子实体时反向借用面板。
    ///
    /// @author kongweiguang
    pub(super) fn open_quick_open(
        &mut self,
        mode: QuickOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.quick_open_view.clone() else {
            self.set_quick_open_status("快速打开面板尚未装配。", "Quick Open is unavailable.", cx);
            return;
        };
        let context = self.quick_open_open_context();
        view.update(cx, |view, cx| view.open(mode, context, window, cx));
        self.quick_open_modal_open = true;
    }

    /// 从当前 reducer/runtime 独立捕获动作 binding 和 clean-prompt 上下文，搜索不受 prompt 限制。
    ///
    /// @author kongweiguang
    fn quick_open_open_context(&self) -> QuickOpenOpenContext {
        let workspace = self
            .workspace_controller
            .as_ref()
            .map(|controller| controller.snapshot().clone());
        let terminal_binding = self.focused_terminal_session_id().and_then(|session_id| {
            let registration = self.pane_input_handlers.get(&session_id)?;
            let binding = registration.binding.clone();
            let owner_is_active = match &binding.owner {
                SessionOwner::Ui { .. } => self.is_active_suggestion_binding(&binding),
                SessionOwner::Agent { agent_session_id } => {
                    self.focused_agent_terminal
                        && self.selected_agent_session_id.as_ref() == Some(agent_session_id)
                        && self.agent_terminal_session_id.as_ref() == Some(&binding.session_id)
                }
                SessionOwner::Mcp | SessionOwner::Service { .. } => false,
            };
            (owner_is_active && binding.session_id == session_id).then_some(binding)
        });
        let terminal = terminal_binding.as_ref().and_then(|binding| {
            let context = self.suggestion_context_for_binding(binding)?;
            (context.prompt_line_empty
                && context.session_id == binding.session_id
                && context.owner == binding.owner
                && context.session_generation == binding.session_generation)
                .then_some(QuickOpenTerminalTarget {
                    binding: binding.clone(),
                    context,
                })
        });
        QuickOpenOpenContext {
            workspace,
            terminal,
            terminal_binding,
        }
    }

    /// 将 typed 选择还原为现有操作；终端动作沿用打开面板时捕获的 binding，禁止失效后转投。
    /// 事件在实体更新结束后处理；若面板已重开，旧关闭事件不能解除新一轮模态门禁。
    ///
    /// @author kongweiguang
    pub(super) fn handle_quick_open_event(
        &mut self,
        event: &QuickOpenEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .quick_open_view
            .as_ref()
            .is_some_and(|view| view.read(cx).is_open())
        {
            return;
        }
        self.quick_open_modal_open = false;
        match event {
            QuickOpenEvent::Dismissed { origin_binding } => {
                self.restore_quick_open_focus_or_root(origin_binding.as_ref(), window, cx);
            }
            QuickOpenEvent::Selected {
                selection,
                origin,
                terminal_binding,
            } => match selection {
                QuickOpenSelection::Action(action) => {
                    if *action == crate::keybindings::KeybindingAction::TerminalSearch {
                        let Some(binding) = terminal_binding.as_ref() else {
                            self.set_quick_open_status(
                                "打开命令面板时没有捕获到终端目标。",
                                "No terminal target was captured when the palette opened.",
                                cx,
                            );
                            return;
                        };
                        if !self.open_terminal_search_for_binding(binding, window, cx) {
                            self.set_quick_open_status(
                                "原终端目标已关闭或重连，未切换到其它终端。",
                                "The captured terminal closed or reconnected; no other terminal was selected.",
                                cx,
                            );
                        }
                    } else if *action == crate::keybindings::KeybindingAction::TerminalPaste {
                        let Some(binding) = terminal_binding.as_ref() else {
                            self.set_quick_open_status(
                                "打开命令面板时没有捕获到终端目标。",
                                "No terminal target was captured when the palette opened.",
                                cx,
                            );
                            return;
                        };
                        if !self.restore_quick_open_focus(binding, window, cx) {
                            self.set_quick_open_status(
                                "原终端目标已关闭或重连，未切换到其它终端。",
                                "The captured terminal closed or reconnected; no other terminal was selected.",
                                cx,
                            );
                            return;
                        }
                        let Some(input_handler) = self
                            .pane_input_handlers
                            .get(&binding.session_id)
                            .filter(|registration| registration.binding == *binding)
                            .map(|registration| registration.entity.clone())
                        else {
                            self.set_quick_open_status(
                                "原终端输入目标已不可用，未切换到其它终端。",
                                "The captured terminal input target is unavailable; no other terminal was selected.",
                                cx,
                            );
                            return;
                        };
                        if !terminal_keyboard::paste_bound_terminal(
                            self,
                            binding,
                            input_handler,
                            cx,
                        ) {
                            self.set_quick_open_status(
                                "原终端输入目标已不可用，剪贴板内容未能粘贴。",
                                "The captured terminal input target is unavailable; clipboard text was not pasted.",
                                cx,
                            );
                        }
                    } else {
                        if matches!(
                            *action,
                            crate::keybindings::KeybindingAction::SettingsOpen
                                | crate::keybindings::KeybindingAction::SettingsKeybindings
                        ) {
                            // Preferences modal 保存 return focus 前先把焦点移出即将卸载的 Quick Open 输入框。
                            self.restore_quick_open_focus_or_root(
                                terminal_binding.as_ref(),
                                window,
                                cx,
                            );
                        }
                        self.dispatch_keybinding_action(*action, window, cx);
                    }
                }
                QuickOpenSelection::Host(host_id) => {
                    self.select_quick_open_host(host_id, window, cx);
                }
                QuickOpenSelection::Tab(tab_id) => {
                    self.activate_quick_open_tab(tab_id, window, cx);
                }
                QuickOpenSelection::Pane { tab_id, pane_id } => {
                    self.focus_quick_open_pane(tab_id, pane_id, window, cx);
                }
                QuickOpenSelection::CurrentDirectory {
                    tab_id,
                    pane_id,
                    path,
                } => self.focus_quick_open_directory(tab_id, pane_id, path, window, cx),
                QuickOpenSelection::WorkspaceFile(tab_id) => {
                    self.activate_quick_open_file(tab_id, window, cx);
                }
                QuickOpenSelection::HistoryCommand(command) => {
                    self.insert_quick_open_history(command, origin.as_deref(), window, cx);
                }
                QuickOpenSelection::Snippet(id) => {
                    self.open_quick_open_snippet(id, window, cx);
                }
                QuickOpenSelection::Workflow(id) => {
                    self.open_quick_open_workflow(id, window, cx);
                }
                QuickOpenSelection::Agent(id) => {
                    self.select_quick_open_agent(id.clone(), window, cx);
                }
            },
        }
    }

    /// 关闭面板时仅恢复仍属于原 UI pane 或 Agent session incarnation 的焦点。
    ///
    /// @author kongweiguang
    fn restore_quick_open_focus_or_root(
        &mut self,
        binding: Option<&UiSessionBinding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !binding.is_some_and(|binding| self.restore_quick_open_focus(binding, window, cx)) {
            // 搜索框会随面板/设置模态卸载；无效目标时用常驻 Root handle，避免恢复到已销毁控件。
            window.focus(&self.focus_handle, cx);
        }
    }

    /// 只在原 owner 与 session generation 仍有效时恢复终端焦点，避免焦点转到重连后的同 ID 会话。
    ///
    /// @author kongweiguang
    fn restore_quick_open_focus(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let target_is_current = match &binding.owner {
            SessionOwner::Ui { .. } => self.is_active_suggestion_binding(binding),
            SessionOwner::Agent { agent_session_id } => {
                self.focused_agent_terminal
                    && self.selected_agent_session_id.as_ref() == Some(agent_session_id)
                    && self.agent_terminal_session_id.as_ref() == Some(&binding.session_id)
                    && self
                        .pane_input_handlers
                        .get(&binding.session_id)
                        .is_some_and(|registration| registration.binding == *binding)
            }
            SessionOwner::Mcp | SessionOwner::Service { .. } => false,
        };
        if !target_is_current {
            return false;
        }
        let Some(registration) = self.pane_input_handlers.get(&binding.session_id) else {
            return false;
        };
        if registration.binding != *binding {
            return false;
        }
        window.focus(&registration.entity.read(cx).focus_handle(), cx);
        self.set_reported_focus_session(Some(binding.session_id.clone()), cx);
        true
    }

    /// Host 选择只进入现有连接详情，不自动连接或从面板结果推断认证意图。
    fn select_quick_open_host(
        &mut self,
        host_id: &kerminal_domain::identity::HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.connections_panel.clone() else {
            self.set_quick_open_status(
                "连接面板尚未装配。",
                "The connections panel is unavailable.",
                cx,
            );
            return;
        };
        self.set_center_panel(CenterPanel::Connections, window, cx);
        panel.update(cx, |panel, cx| panel.select_host_id(host_id, cx));
    }

    /// 按当前 workspace 中的精确 TabId 激活标签，过期搜索结果不会映射到相邻标签；只有 reducer 接受后才同步 Root 可见页。
    ///
    /// @author kongweiguang
    fn activate_quick_open_tab(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let exists = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .any(|tab| tab.id() == tab_id)
            });
        if !exists {
            self.set_quick_open_status(
                "该工作区标签已关闭，请重新搜索。",
                "That workspace tab has closed; search again.",
                cx,
            );
            return;
        }
        let event = crate::features::workspace_tabs::WorkspaceTabEvent::Activate(tab_id.clone());
        if self.handle_workspace_tab_event(&event, window, cx) {
            self.activate_workspace_tab(tab_id, window, cx);
        }
    }

    /// 只聚焦当前 terminal layout 确认存在的 pane；目标状态变化时不切到其它 session。
    fn focus_quick_open_pane(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let exists = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| {
                controller
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .find(|tab| tab.id() == tab_id)
                    .and_then(WorkspaceSessionTab::terminal_layout)
            })
            .is_some_and(|layout| layout.contains_pane(pane_id));
        if !exists {
            self.set_quick_open_status(
                "该终端分屏已不存在，请重新搜索。",
                "That terminal pane no longer exists; search again.",
                cx,
            );
            return;
        }
        self.focus_workspace_pane(tab_id.clone(), pane_id.clone(), window, cx);
    }

    /// 当前目录结果只导航到对应 pane，不向 shell 写入 cd 或执行任何命令。
    fn focus_quick_open_directory(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let matches_current = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| {
                controller
                    .snapshot()
                    .terminal_panes
                    .iter()
                    .find(|pane| &pane.id == pane_id)
            })
            .is_some_and(|pane| {
                pane.current_cwd.as_deref().or(pane.cwd.as_deref()) == Some(path)
                    && self
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| {
                            controller
                                .snapshot()
                                .terminal_tabs
                                .iter()
                                .find(|tab| tab.id() == tab_id)
                                .and_then(WorkspaceSessionTab::terminal_layout)
                        })
                        .is_some_and(|layout| layout.contains_pane(pane_id))
            });
        if !matches_current {
            self.set_quick_open_status(
                "终端目录已变化，请重新搜索。",
                "The terminal directory changed; search again.",
                cx,
            );
            return;
        }
        self.focus_workspace_pane(tab_id.clone(), pane_id.clone(), window, cx);
    }

    /// 只打开仍存在的 WorkspaceFile descriptor，并沿用 workspace 的文件脏状态门禁。
    fn activate_quick_open_file(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_file = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller.snapshot().terminal_tabs.iter().any(|tab| {
                    tab.id() == tab_id && matches!(tab, WorkspaceSessionTab::WorkspaceFile { .. })
                })
            });
        if !is_file {
            self.set_quick_open_status(
                "该工作区文件已关闭，请重新搜索。",
                "That workspace file is no longer open; search again.",
                cx,
            );
            return;
        }
        self.activate_quick_open_tab(tab_id, window, cx);
    }

    /// 历史命令仅作为待编辑文本插入，并用打开时 context version 原子拒绝旧候选；FIFO 接管同一 work Box。
    ///
    /// @author kongweiguang
    fn insert_quick_open_history(
        &mut self,
        command: &str,
        origin: Option<&QuickOpenTerminalTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(origin) = origin.filter(|origin| {
            origin.context.prompt_line_empty
                && origin.context.availability == TerminalCommandAvailability::Available
                && origin.binding.session_id == origin.context.session_id
                && origin.binding.owner == origin.context.owner
                && origin.binding.session_generation == origin.context.session_generation
        }) else {
            self.set_quick_open_status(
                "需要原始终端仍处于空白且可信的提示符。",
                "The original terminal must still have the same clean, trusted prompt.",
                cx,
            );
            return;
        };
        let is_current = self
            .suggestion_context_for_binding(&origin.binding)
            .is_some_and(|context| {
                context.version() == origin.context.version() && context.prompt_line_empty
            })
            && self
                .pane_input_handlers
                .get(&origin.binding.session_id)
                .is_some_and(|registration| registration.binding == origin.binding);
        if !is_current || !history_command_is_safe(command) {
            self.set_quick_open_status(
                "命令或终端目标已变化，文本未插入。",
                "The command or terminal target changed; no text was inserted.",
                cx,
            );
            return;
        }

        if let Some(registration) = self.pane_input_handlers.get(&origin.binding.session_id) {
            window.focus(&registration.entity.read(cx).focus_handle(), cx);
        }
        self.set_reported_focus_session(Some(origin.binding.session_id.clone()), cx);
        let request = UiTerminalInputRequest {
            binding: origin.binding.clone(),
            expected_context: Some(origin.context.version()),
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Text(command.to_owned()),
        };
        if !self.enqueue_session_command(
            SessionCommand::UiInput(Box::new(super::session_queue::UiInputWork::new(
                request, None,
            ))),
            cx,
        ) {
            self.set_quick_open_status(
                "终端输入队列未接受命令文本。",
                "The terminal input queue rejected the command text.",
                cx,
            );
        }
    }

    /// 打开片段工具后按稳定 ID 导航；同一 Window 沿宿主入口传递，不绕过编辑器保护。
    ///
    /// @author kongweiguang
    fn open_quick_open_snippet(
        &mut self,
        id: &str,
        cx_window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_tool_panel_visible(ToolRailToolId::Snippets, cx_window, cx);
        let Some(commands) = self.commands_view.clone() else {
            self.set_quick_open_status(
                "命令片段服务尚未装配。",
                "The snippet editor is unavailable.",
                cx,
            );
            return;
        };
        let id = id.to_owned();
        commands.update(cx, |view, cx| view.open_snippet_by_id(id, cx_window, cx));
    }

    /// 工作流选择沿当前 Window 打开既有宿主，继续使用 versioned editor 的脏状态转场。
    ///
    /// @author kongweiguang
    fn open_quick_open_workflow(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_tool_panel_visible(ToolRailToolId::Snippets, window, cx);
        let Some(commands) = self.commands_view.clone() else {
            self.set_quick_open_status(
                "工作流编辑器尚未装配。",
                "The workflow editor is unavailable.",
                cx,
            );
            return;
        };
        let id = id.to_owned();
        commands.update(cx, |view, cx| view.open_workflow_by_id(id, window, cx));
    }

    /// 显示 Agent 宿主时沿用当前 Window，仍只选择已验证列表项，不能借 ID 重建或恢复会话。
    ///
    /// @author kongweiguang
    fn select_quick_open_agent(
        &mut self,
        id: kerminal_domain::identity::AgentSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_tool_panel_visible(ToolRailToolId::AgentLauncher, window, cx);
        let Some(panel) = self.agents_panel.clone() else {
            self.set_quick_open_status(
                "Agent 服务尚未装配。",
                "The Agent service is unavailable.",
                cx,
            );
            return;
        };
        let _ = panel.update(cx, |panel, cx| panel.select_existing_agent(id, cx));
    }

    /// 只沿现有 Window 打开未显示的工具，保持此入口不关闭面板、也不触发隐藏后的焦点恢复。
    ///
    /// @author kongweiguang
    fn ensure_tool_panel_visible(
        &mut self,
        tool: ToolRailToolId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self
            .settings_snapshot
            .as_ref()
            .map(|snapshot| snapshot.tool_rail.clone())
            .unwrap_or_default();
        let slots = self.tool_rail_state.project(&settings).panels;
        let mounted = slots.attached == Some(tool)
            || slots.left == Some(tool)
            || slots.bottom == Some(tool)
            || slots.center == Some(tool);
        if !mounted || (slots.attached == Some(tool) && self.right_dock_collapsed) {
            self.toggle_tool_rail_panel(tool, window, cx);
        }
    }

    /// 用双语状态显示目标过期或服务缺失，不把静默 no-op 当成导航成功。
    ///
    /// @author kongweiguang
    fn set_quick_open_status(
        &mut self,
        chinese: &'static str,
        english: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(crate::locale::text(chinese, english));
        cx.notify();
    }
}

/// 历史正文不允许控制字符或多行输入，面板选择后也不会自动提交 Enter。
fn history_command_is_safe(command: &str) -> bool {
    !command.trim().is_empty()
        && command.chars().count() <= 8_000
        && command.len() <= 32_000
        && !command.chars().any(char::is_control)
}
