//! Root 快捷键动作分发；只复用已有 view/entity 和 workspace reducer，不复制资源状态。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::{preferences::ToolRailToolId, workspace::WorkspaceSessionTab};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::SplitDirection,
};

use crate::{
    features::{
        workspace_panes::WorkspacePaneEvent,
        workspace_tabs::{
            WorkspaceTabEvent, WorkspaceTabKind, WorkspaceTabProjection,
            append_shared_sftp_workbench_projection,
        },
    },
    keybindings::KeybindingAction,
};

use super::{
    CenterPanel, QuitIntent, RootView, TerminalTargetRequest, terminal_input_modal_blocked,
    terminal_keyboard, workspace_render::WorkspaceRenderEvent,
};

impl RootView {
    /// 将快捷键交给同一宿主或 reducer；TerminalPaste 还必须服从 Root 的 Quick Open 模态镜像，
    /// 而面板选择事件会先解除该镜像再显式执行粘贴，避免既漏到底层也误阻合法动作。
    ///
    /// @author kongweiguang
    pub(super) fn dispatch_keybinding_action(
        &mut self,
        action: KeybindingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match action {
            KeybindingAction::ApplicationQuit => {
                self.request_quit(QuitIntent::Exit, cx);
            }
            KeybindingAction::WorkspaceQuickOpen => {
                self.open_quick_open(
                    crate::features::quick_open::QuickOpenMode::QuickOpen,
                    window,
                    cx,
                );
            }
            KeybindingAction::WorkspaceCommandPalette => {
                self.open_quick_open(
                    crate::features::quick_open::QuickOpenMode::CommandPalette,
                    window,
                    cx,
                );
            }
            KeybindingAction::SettingsOpen => {
                self.set_center_panel(CenterPanel::Preferences, window, cx);
            }
            KeybindingAction::SettingsKeybindings => {
                self.set_center_panel(CenterPanel::Preferences, window, cx);
                self.preferences_panel
                    .update(cx, |panel, cx| panel.select_keybindings(cx));
            }
            KeybindingAction::TerminalFocus => {
                self.set_center_panel(CenterPanel::Terminal, window, cx);
            }
            KeybindingAction::TerminalSearch => {
                let Some(session_id) = self.focused_terminal_session_id() else {
                    self.status = Some(crate::locale::text(
                        "当前没有可搜索的终端。",
                        "There is no focused terminal to search.",
                    ));
                    cx.notify();
                    return true;
                };
                let Some(binding) = self
                    .pane_input_handlers
                    .get(&session_id)
                    .map(|registration| registration.binding.clone())
                else {
                    self.status = Some(crate::locale::text(
                        "当前终端输入目标尚未就绪。",
                        "The focused terminal input target is not ready.",
                    ));
                    cx.notify();
                    return true;
                };
                if !self.open_terminal_search_for_binding(&binding, window, cx) {
                    self.set_action_status(
                        "终端搜索目标已失效或不可用。",
                        "The terminal search target is stale or unavailable.",
                        cx,
                    );
                }
            }
            KeybindingAction::TerminalPaste => {
                if terminal_input_modal_blocked(false, self.quick_open_modal_open) {
                    return true;
                }
                let Some(binding) = self.logical_focused_pane_binding(cx) else {
                    self.set_action_status(
                        "当前没有可粘贴的终端输入目标。",
                        "There is no focused terminal input target for pasting.",
                        cx,
                    );
                    return true;
                };
                let Some(input_handler) = self
                    .pane_input_handlers
                    .get(&binding.session_id)
                    .filter(|registration| registration.binding == binding)
                    .map(|registration| registration.entity.clone())
                else {
                    self.set_action_status(
                        "当前终端输入目标尚未就绪。",
                        "The focused terminal input target is not ready.",
                        cx,
                    );
                    return true;
                };
                if !terminal_keyboard::paste_bound_terminal(self, &binding, input_handler, cx) {
                    self.set_action_status(
                        "当前终端输入目标已失效，剪贴板内容未能粘贴。",
                        "The focused terminal input target is stale; clipboard text was not pasted.",
                        cx,
                    );
                }
            }
            KeybindingAction::TerminalNewTab => {
                self.create_terminal_for_target(
                    TerminalTargetRequest::ProfileDefaultLocal,
                    window,
                    cx,
                );
            }
            KeybindingAction::TerminalCloseTab => {
                if self.shared_sftp_workbench_is_selected() {
                    let tab_id = self.sftp_workbench_tab_id.clone();
                    self.request_workspace_tab_close_with_confirmation(tab_id, window, cx);
                    return true;
                }
                let Some(tab_id) = self.active_terminal_tab_id() else {
                    self.set_action_status(
                        "当前没有可关闭的终端标签。",
                        "There is no active terminal tab to close.",
                        cx,
                    );
                    return true;
                };
                self.request_workspace_tab_close_with_confirmation(tab_id, window, cx);
            }
            KeybindingAction::TerminalClosePane => {
                let Some((tab_id, pane_id)) = self.active_terminal_pane() else {
                    self.set_action_status(
                        "当前没有可关闭的终端分屏。",
                        "There is no active terminal pane to close.",
                        cx,
                    );
                    return true;
                };
                self.handle_workspace_render_event(
                    WorkspaceRenderEvent::Pane {
                        tab_id,
                        event: WorkspacePaneEvent::Close(pane_id),
                    },
                    window,
                    cx,
                );
            }
            KeybindingAction::TerminalSplitHorizontal | KeybindingAction::TerminalSplitVertical => {
                let Some((tab_id, pane_id)) = self.active_terminal_pane() else {
                    self.set_action_status(
                        "当前没有可拆分的终端分屏。",
                        "There is no active terminal pane to split.",
                        cx,
                    );
                    return true;
                };
                let direction = if action == KeybindingAction::TerminalSplitHorizontal {
                    SplitDirection::Horizontal
                } else {
                    SplitDirection::Vertical
                };
                self.handle_workspace_render_event(
                    WorkspaceRenderEvent::Pane {
                        tab_id,
                        event: WorkspacePaneEvent::Split {
                            pane_id,
                            direction,
                            before: false,
                        },
                    },
                    window,
                    cx,
                );
            }
            KeybindingAction::TerminalPreviousTab => {
                self.activate_adjacent_workspace_tab(true, window, cx);
            }
            KeybindingAction::TerminalNextTab => {
                self.activate_adjacent_workspace_tab(false, window, cx);
            }
            KeybindingAction::ToolAgentLauncher => {
                self.activate_tool_rail_panel_from_shortcut(
                    ToolRailToolId::AgentLauncher,
                    window,
                    cx,
                );
            }
            KeybindingAction::ToolSystem => {
                self.activate_tool_rail_panel_from_shortcut(ToolRailToolId::System, window, cx);
            }
            KeybindingAction::ToolSftp => {
                self.activate_tool_rail_panel_from_shortcut(ToolRailToolId::Sftp, window, cx);
            }
            KeybindingAction::ToolPorts => {
                self.activate_tool_rail_panel_from_shortcut(ToolRailToolId::Ports, window, cx);
            }
            KeybindingAction::ToolSnippets => {
                self.activate_tool_rail_panel_from_shortcut(ToolRailToolId::Snippets, window, cx);
            }
            KeybindingAction::ToolLogs => {
                self.activate_tool_rail_panel_from_shortcut(ToolRailToolId::Logs, window, cx);
            }
        }
        true
    }

    /// 按可见 Tab 解析 reducer 中真实 Terminal；共享 SFTP 工作台显示时不能误关底层 active 终端。
    ///
    /// @author kongweiguang
    fn active_terminal_tab_id(&self) -> Option<TabId> {
        let snapshot = self.workspace_controller.as_ref()?.snapshot();
        let tab_id = self.visible_workspace_tab_id()?;
        snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == &tab_id && matches!(tab, WorkspaceSessionTab::Terminal { .. }))
            .map(|tab| tab.id().clone())
    }

    /// 只从可见 Terminal 的 layout 选取有效焦点 pane；临时文件 Tab 不借用后台 controller 的 pane 焦点。
    ///
    /// @author kongweiguang
    fn active_terminal_pane(&self) -> Option<(TabId, PaneId)> {
        let snapshot = self.workspace_controller.as_ref()?.snapshot();
        let tab_id = self.visible_workspace_tab_id()?;
        let tab = snapshot.terminal_tabs.iter().find(|tab| {
            tab.id() == &tab_id && matches!(tab, WorkspaceSessionTab::Terminal { .. })
        })?;
        let layout = tab.terminal_layout()?;
        let pane_id = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| layout.contains_pane(pane_id))
            .cloned()
            .or_else(|| layout.pane_ids().into_iter().next())?;
        Some((tab_id, pane_id))
    }

    /// 按标签栏实际投影顺序环绕切换，保留折叠组成员和临时工作台的可见身份语义。
    /// Controller 负责持久标签顺序，Root 只追加当前打开的共享工作台；临时项经现有事件路由激活，
    /// 持久项仅在 reducer 接受后同步 Root 可见页，单项循环不重复触发焦点或资源副作用。
    ///
    /// @author kongweiguang
    fn activate_adjacent_workspace_tab(
        &mut self,
        previous: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_controller.is_none() && !self.sftp_workbench_tab_open {
            self.set_action_status("工作区尚未加载。", "The workspace has not loaded yet.", cx);
            return;
        }
        let mut projections = self
            .workspace_controller
            .as_ref()
            .map(|controller| controller.tab_projections())
            .unwrap_or_default();
        if self.sftp_workbench_tab_open {
            append_shared_sftp_workbench_projection(
                &mut projections,
                &self.sftp_workbench_tab_id,
                self.shared_sftp_workbench_is_selected(),
            );
        }
        let active_tab_id = self.visible_workspace_tab_id();
        let Some(tab_id) =
            adjacent_workspace_tab_id(&projections, active_tab_id.as_ref(), previous)
        else {
            self.set_action_status(
                "工作区中没有可切换的标签。",
                "There are no workspace tabs to switch to.",
                cx,
            );
            return;
        };
        if active_tab_id.as_ref() == Some(&tab_id) {
            return;
        }

        let is_temporary_workbench = projections.iter().any(|projection| {
            projection.id == tab_id && matches!(&projection.kind, WorkspaceTabKind::SftpWorkbench)
        });
        let activation_accepted = self.handle_workspace_tab_event(
            &WorkspaceTabEvent::Activate(tab_id.clone()),
            window,
            cx,
        );
        if activation_accepted && !is_temporary_workbench {
            self.activate_workspace_tab(&tab_id, window, cx);
        }
    }

    /// 缺少服务或工作区时给出双语可见状态，动作仍视为已处理以免按键落入 PTY。
    fn set_action_status(
        &mut self,
        zh_cn: &'static str,
        en_us: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(crate::locale::text(zh_cn, en_us));
        cx.notify();
    }
}

/// 只依据 Root 提供的 UI 投影选择相邻身份；空集合返回 None，无活动项时从首/末项进入。
/// 折叠组成员仍保留在 projection 中，因而活动成员即使暂时不可见也能按其真实顺序继续循环。
///
/// @author kongweiguang
fn adjacent_workspace_tab_id(
    projections: &[WorkspaceTabProjection],
    active_tab_id: Option<&TabId>,
    previous: bool,
) -> Option<TabId> {
    let count = projections.len();
    if count == 0 {
        return None;
    }

    let current_index = active_tab_id.and_then(|active| {
        projections
            .iter()
            .position(|projection| &projection.id == active)
    });
    let next_index = match (current_index, previous) {
        (Some(0), true) => count - 1,
        (Some(index), true) => index - 1,
        (Some(index), false) if index == count - 1 => 0,
        (Some(index), false) => index + 1,
        (None, true) => count - 1,
        (None, false) => 0,
    };

    Some(projections[next_index].id.clone())
}

#[cfg(test)]
#[path = "../../tests/unit/root_keybinding_actions.rs"]
mod tests;
