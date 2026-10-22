//! 终端 Canvas 右键动作；点击始终回到菜单打开时冻结的 session owner。
//!
//! @author kongweiguang

use gpui_kit::{
    App, Context, Entity, Window,
    component::menu::{PopupMenu, PopupMenuItem},
};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{sessions::SessionOwner, workspace::SplitDirection};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};

use crate::{
    features::terminal_tools::{TerminalToolsEvent, TerminalToolsView},
    features::workspace_panes::WorkspacePaneEvent,
    root::{
        RootView, RuntimePanePhase, pane_input::PaneTerminalInputHandler,
        workspace_render::WorkspaceRenderEvent,
    },
    terminal_interaction::TerminalRightClickAction,
};

/// 菜单动作绑定打开时的 session owner；搜索、全选和输入都不会按之后的焦点重新选目标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalContextMenuAction {
    CopySelection,
    ClearSelection,
    SelectAll,
    Paste,
    Search,
    ClearScreen,
    Split {
        direction: SplitDirection,
        before: bool,
    },
    ChooseSplitTarget {
        direction: SplitDirection,
        before: bool,
    },
}

/// 菜单闭包要区分目标过期与动作不再适用，避免静默拒绝或把失败说成已执行。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalContextMenuOutcome {
    Completed,
    TargetChanged,
    SelectionChanged,
    InputRejected,
}

/// 创建 kit popup 的实际菜单项；分屏动作明确携带四向位置，按主机入口使用同一冻结来源 pane。
///
/// @author kongweiguang
pub(crate) fn build_terminal_context_menu(
    mut menu: PopupMenu,
    root: Entity<RootView>,
    input_handler: Option<Entity<PaneTerminalInputHandler>>,
    tools: Entity<TerminalToolsView>,
    binding: UiSessionBinding,
    has_selection: bool,
    _cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    if has_selection {
        menu = menu
            .item(context_menu_item(
                crate::locale::text("复制", "Copy"),
                root.clone(),
                input_handler.clone(),
                tools.clone(),
                binding.clone(),
                TerminalContextMenuAction::CopySelection,
            ))
            .item(context_menu_item(
                crate::locale::text("清除选择", "Clear Selection"),
                root.clone(),
                input_handler.clone(),
                tools.clone(),
                binding.clone(),
                TerminalContextMenuAction::ClearSelection,
            ))
            .separator();
    }

    menu = menu.item(context_menu_item(
        crate::locale::text("全选", "Select All"),
        root.clone(),
        input_handler.clone(),
        tools.clone(),
        binding.clone(),
        TerminalContextMenuAction::SelectAll,
    ));
    menu = menu.separator();
    if let Some(input_handler) = input_handler.as_ref() {
        menu = menu.item(context_menu_item(
            crate::locale::text("粘贴", "Paste"),
            root.clone(),
            Some(input_handler.clone()),
            tools.clone(),
            binding.clone(),
            TerminalContextMenuAction::Paste,
        ));
    }
    menu = menu.item(context_menu_item(
        crate::locale::text("搜索终端", "Search Terminal"),
        root.clone(),
        input_handler.clone(),
        tools.clone(),
        binding.clone(),
        TerminalContextMenuAction::Search,
    ));
    if let Some(input_handler) = input_handler.as_ref() {
        menu = menu.separator().item(context_menu_item(
            crate::locale::text("发送 Ctrl+L", "Send Ctrl+L"),
            root.clone(),
            Some(input_handler.clone()),
            tools.clone(),
            binding.clone(),
            TerminalContextMenuAction::ClearScreen,
        ));
    }
    menu = menu.separator();
    for (action, zh, en) in terminal_split_menu_actions() {
        menu = menu.item(context_menu_item(
            crate::locale::text(zh, en),
            root.clone(),
            input_handler.clone(),
            tools.clone(),
            binding.clone(),
            action,
        ));
    }
    menu
}

/// 右键菜单把四向直接分屏和四向主机选择放在一起，避免只支持两个默认插入方向。
///
/// @author kongweiguang
fn terminal_split_menu_actions() -> [(TerminalContextMenuAction, &'static str, &'static str); 8] {
    use SplitDirection::{Horizontal, Vertical};
    [
        (
            TerminalContextMenuAction::Split {
                direction: Horizontal,
                before: false,
            },
            "向右分屏",
            "Split right",
        ),
        (
            TerminalContextMenuAction::Split {
                direction: Horizontal,
                before: true,
            },
            "向左分屏",
            "Split left",
        ),
        (
            TerminalContextMenuAction::Split {
                direction: Vertical,
                before: false,
            },
            "向下分屏",
            "Split below",
        ),
        (
            TerminalContextMenuAction::Split {
                direction: Vertical,
                before: true,
            },
            "向上分屏",
            "Split above",
        ),
        (
            TerminalContextMenuAction::ChooseSplitTarget {
                direction: Horizontal,
                before: false,
            },
            "选择主机并向右分屏…",
            "Choose host and split right…",
        ),
        (
            TerminalContextMenuAction::ChooseSplitTarget {
                direction: Horizontal,
                before: true,
            },
            "选择主机并向左分屏…",
            "Choose host and split left…",
        ),
        (
            TerminalContextMenuAction::ChooseSplitTarget {
                direction: Vertical,
                before: false,
            },
            "选择主机并向下分屏…",
            "Choose host and split below…",
        ),
        (
            TerminalContextMenuAction::ChooseSplitTarget {
                direction: Vertical,
                before: true,
            },
            "选择主机并向上分屏…",
            "Choose host and split above…",
        ),
    ]
}

/// 绑定菜单点击到打开菜单时的输入实体和工具实体，异步处理不得重新读取活动 pane。
///
/// @author kongweiguang
fn context_menu_item(
    label: impl Into<gpui_kit::SharedString>,
    root: Entity<RootView>,
    input_handler: Option<Entity<PaneTerminalInputHandler>>,
    tools: Entity<TerminalToolsView>,
    binding: UiSessionBinding,
    action: TerminalContextMenuAction,
) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        let binding = binding.clone();
        let input_handler = input_handler.clone();
        let tools = tools.clone();
        root.update(cx, |root, cx| {
            let outcome = root.handle_terminal_context_menu_action(
                binding,
                input_handler,
                tools,
                action,
                window,
                cx,
            );
            let (chinese, english) = match outcome {
                TerminalContextMenuOutcome::Completed => return,
                TerminalContextMenuOutcome::TargetChanged => (
                    "终端目标已变化，请重新打开菜单。",
                    "The terminal target changed; reopen the context menu.",
                ),
                TerminalContextMenuOutcome::SelectionChanged => (
                    "终端选择已变化，请重新打开菜单。",
                    "The terminal selection changed; reopen the context menu.",
                ),
                TerminalContextMenuOutcome::InputRejected => (
                    "终端未接受该输入，请确认目标仍可用后重试。",
                    "The terminal rejected this input; verify the target and retry.",
                ),
            };
            root.status = Some(crate::locale::text(chinese, english));
            cx.notify();
        });
    })
}

impl RootView {
    /// 菜单打开只确认原 pane 仍有效；只有焦点或输入动作才激活它，避免右键改变工作区位置。
    ///
    /// @author kongweiguang
    pub(crate) fn handle_terminal_canvas_right_click(
        &mut self,
        binding: UiSessionBinding,
        input_handler: Option<Entity<PaneTerminalInputHandler>>,
        tools: Entity<TerminalToolsView>,
        action: TerminalRightClickAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let target_is_registered = if action == TerminalRightClickAction::Menu {
            terminal_context_view_target_is_registered(self, &binding, &tools, cx)
        } else {
            input_handler.as_ref().is_some_and(|input_handler| {
                terminal_context_target_is_registered(self, &binding, input_handler, &tools, cx)
            })
        };
        if !target_is_registered {
            return false;
        }
        if terminal_right_click_requires_focus(action) {
            let Some(input_handler) = input_handler.as_ref() else {
                return false;
            };
            if !self.focus_terminal_canvas_target(
                &binding,
                input_handler.clone(),
                tools.clone(),
                window,
                cx,
            ) {
                return false;
            }
        }

        match action {
            TerminalRightClickAction::Menu | TerminalRightClickAction::FocusOnly => true,
            TerminalRightClickAction::Paste => {
                let Some(input_handler) = input_handler else {
                    return false;
                };
                if !terminal_context_target_is_current(self, &binding, &input_handler, &tools, cx) {
                    return false;
                }
                let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
                    return true;
                };
                if text.is_empty() {
                    return true;
                }
                if !terminal_context_target_is_current(self, &binding, &input_handler, &tools, cx) {
                    return false;
                }
                input_handler.update(cx, |handler, cx| {
                    handler.emit_input_event(TerminalInputEvent::Paste(text), cx)
                })
            }
        }
    }

    /// 鼠标输入前激活精确 pane；同 binding 已实际持有焦点时跳过重复持久化和快照请求。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_terminal_canvas_target(
        &mut self,
        binding: &UiSessionBinding,
        input_handler: Entity<PaneTerminalInputHandler>,
        tools: Entity<TerminalToolsView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !terminal_context_target_is_registered(self, binding, &input_handler, &tools, cx) {
            return false;
        }
        let already_focused = self.focused_terminal_session_id().as_ref()
            == Some(&binding.session_id)
            && input_handler.read(cx).focus_handle().is_focused(window);
        if already_focused {
            return true;
        }
        self.focus_terminal_context_target(binding, input_handler.clone(), window, cx)
            && terminal_context_target_is_current(self, binding, &input_handler, &tools, cx)
    }

    /// 菜单动作再次校验打开时的 session、owner、generation 和实体身份后复用现有工具入口。
    /// 分屏前将焦点锚回来源终端，使菜单关闭后的既有 pending-focus 校验仍能识别同一用户意图。
    ///
    /// @author kongweiguang
    fn handle_terminal_context_menu_action(
        &mut self,
        binding: UiSessionBinding,
        input_handler: Option<Entity<PaneTerminalInputHandler>>,
        tools: Entity<TerminalToolsView>,
        action: TerminalContextMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TerminalContextMenuOutcome {
        let read_only_action = matches!(
            action,
            TerminalContextMenuAction::CopySelection
                | TerminalContextMenuAction::ClearSelection
                | TerminalContextMenuAction::SelectAll
                | TerminalContextMenuAction::Search
                | TerminalContextMenuAction::Split { .. }
                | TerminalContextMenuAction::ChooseSplitTarget { .. }
        );
        let target_is_active = if read_only_action {
            terminal_context_view_target_is_active(self, &binding, &tools, cx)
        } else {
            input_handler.as_ref().is_some_and(|input_handler| {
                terminal_context_menu_target_is_active(self, &binding, input_handler, &tools, cx)
            })
        };
        if !target_is_active {
            return TerminalContextMenuOutcome::TargetChanged;
        }

        if action == TerminalContextMenuAction::ClearScreen
            && let Some(input_handler) = input_handler.as_ref()
            && (!self.focus_terminal_canvas_target(
                &binding,
                input_handler.clone(),
                tools.clone(),
                window,
                cx,
            ) || !terminal_context_target_is_current(self, &binding, input_handler, &tools, cx))
        {
            return TerminalContextMenuOutcome::TargetChanged;
        }

        if split_menu_action_requires_terminal_focus(action)
            && let Some(input_handler) = input_handler.as_ref()
            && (!self.focus_terminal_context_target(&binding, input_handler.clone(), window, cx)
                || !terminal_context_target_is_current(self, &binding, input_handler, &tools, cx))
        {
            return TerminalContextMenuOutcome::TargetChanged;
        }

        match action {
            TerminalContextMenuAction::ClearScreen => {
                let Some(input_handler) = input_handler else {
                    return TerminalContextMenuOutcome::InputRejected;
                };
                if input_handler.update(cx, |handler, cx| {
                    handler.emit_input_event(clear_screen_input_event(), cx)
                }) {
                    TerminalContextMenuOutcome::Completed
                } else {
                    TerminalContextMenuOutcome::InputRejected
                }
            }
            TerminalContextMenuAction::Search => {
                if self.open_terminal_search_for_binding(&binding, window, cx) {
                    TerminalContextMenuOutcome::Completed
                } else {
                    TerminalContextMenuOutcome::TargetChanged
                }
            }
            TerminalContextMenuAction::SelectAll => {
                self.start_select_all_for_binding(binding, tools, cx);
                TerminalContextMenuOutcome::Completed
            }
            TerminalContextMenuAction::Paste => {
                if self.handle_terminal_canvas_right_click(
                    binding,
                    input_handler,
                    tools,
                    TerminalRightClickAction::Paste,
                    window,
                    cx,
                ) {
                    TerminalContextMenuOutcome::Completed
                } else {
                    TerminalContextMenuOutcome::TargetChanged
                }
            }
            TerminalContextMenuAction::Split { .. }
            | TerminalContextMenuAction::ChooseSplitTarget { .. } => {
                let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
                    return TerminalContextMenuOutcome::InputRejected;
                };
                let Some(event) = workspace_pane_event_for_menu_action(action, pane_id.clone())
                else {
                    return TerminalContextMenuOutcome::InputRejected;
                };
                self.handle_workspace_render_event(
                    WorkspaceRenderEvent::Pane {
                        tab_id: tab_id.clone(),
                        event,
                    },
                    window,
                    cx,
                );
                TerminalContextMenuOutcome::Completed
            }
            TerminalContextMenuAction::CopySelection
            | TerminalContextMenuAction::ClearSelection => {
                let has_selection = self.terminal_selection_may_be_present(&binding, &tools, cx);
                if !has_selection {
                    return TerminalContextMenuOutcome::SelectionChanged;
                }
                let event = match action {
                    TerminalContextMenuAction::CopySelection => TerminalToolsEvent::CopySelection {
                        session_id: binding.session_id,
                    },
                    TerminalContextMenuAction::ClearSelection => {
                        TerminalToolsEvent::ClearSelection {
                            session_id: binding.session_id,
                        }
                    }
                    TerminalContextMenuAction::Paste
                    | TerminalContextMenuAction::Search
                    | TerminalContextMenuAction::ClearScreen
                    | TerminalContextMenuAction::SelectAll => {
                        return TerminalContextMenuOutcome::InputRejected;
                    }
                    TerminalContextMenuAction::Split { .. }
                    | TerminalContextMenuAction::ChooseSplitTarget { .. } => {
                        return TerminalContextMenuOutcome::InputRejected;
                    }
                };
                self.handle_terminal_tools_event_from(tools, &event, window, cx);
                TerminalContextMenuOutcome::Completed
            }
        }
    }

    /// 搜索会接管物理焦点，因此先取消已通过完整 owner/generation 校验的原 Canvas 手势与自动滚动链。
    /// 这也覆盖退休历史页：它没有 pane handler 的 blur 订阅，不能依赖普通输入焦点回调结束拖选。
    ///
    /// @author kongweiguang
    pub(super) fn open_terminal_search_for_binding(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let tools = match &binding.owner {
            SessionOwner::Ui { .. } => self.workspace_tools.get(&binding.session_id).cloned(),
            SessionOwner::Agent { .. } => self.agent_terminal_tools.clone(),
            SessionOwner::Mcp | SessionOwner::Service { .. } => None,
        };
        let Some(tools) = tools else {
            return false;
        };
        if !terminal_context_view_target_is_active(self, binding, &tools, cx) {
            return false;
        }
        if let Some(registration) = self.pane_input_handlers.get(&binding.session_id) {
            if registration.binding != *binding {
                return false;
            }
            let input_handler = registration.entity.clone();
            if !terminal_context_target_is_registered(self, binding, &input_handler, &tools, cx)
                || !self.focus_terminal_context_target(binding, input_handler.clone(), window, cx)
                || !terminal_context_target_is_registered(self, binding, &input_handler, &tools, cx)
            {
                return false;
            }
        } else if !self.focus_terminal_history_target(binding, window, cx) {
            return false;
        }
        // 先结束绑定自身的选择/滚动手势，再把焦点交给搜索框，避免历史页失焦后旧边缘滚动继续运行。
        tools.update(cx, |view, cx| view.cancel_canvas_selection_gesture(cx));
        self.terminal_command_suggestions.clear();
        self.suggestion_menu_binding = None;
        self.sync_suggestion_menu(cx);
        cx.notify();
        tools.update(cx, |view, cx| {
            view.open_search_for(&binding.session_id, window, cx)
        })
    }

    /// Escape 仅关闭绑定 pane 的搜索控件，并将焦点还给该代次的终端输入句柄。
    ///
    /// @author kongweiguang
    pub(super) fn close_terminal_search_for_binding(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let tools = match &binding.owner {
            SessionOwner::Ui { .. } => self.workspace_tools.get(&binding.session_id).cloned(),
            SessionOwner::Agent { .. } => self.agent_terminal_tools.clone(),
            SessionOwner::Mcp | SessionOwner::Service { .. } => None,
        };
        let Some(tools) = tools else {
            return false;
        };
        let is_registered =
            if let Some(registration) = self.pane_input_handlers.get(&binding.session_id) {
                registration.binding == *binding
                    && terminal_context_target_is_registered(
                        self,
                        binding,
                        &registration.entity,
                        &tools,
                        cx,
                    )
            } else {
                terminal_context_view_target_is_active(self, binding, &tools, cx)
            };
        if is_registered && tools.read(cx).search_overlay().is_some() {
            tools.update(cx, |view, cx| view.close_search(window, cx));
            return self.restore_terminal_focus_after_search_close(binding, window, cx);
        }
        false
    }

    /// 搜索视图已先行隐藏时，仍按其原 session generation 恢复焦点，不把 Escape 从终端进程截走。
    ///
    /// @author kongweiguang
    pub(super) fn restore_terminal_focus_after_search_close(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let tools = match &binding.owner {
            SessionOwner::Ui { .. } => self.workspace_tools.get(&binding.session_id).cloned(),
            SessionOwner::Agent { .. } => self.agent_terminal_tools.clone(),
            SessionOwner::Mcp | SessionOwner::Service { .. } => None,
        };
        let Some(tools) = tools else {
            return false;
        };
        let Some(registration) = self.pane_input_handlers.get(&binding.session_id) else {
            return terminal_context_view_target_is_active(self, binding, &tools, cx)
                && self.focus_terminal_history_target(binding, window, cx);
        };
        if registration.binding != *binding {
            return false;
        }
        let input_handler = registration.entity.clone();
        if !terminal_context_target_is_registered(self, binding, &input_handler, &tools, cx) {
            return false;
        }
        if !self.focus_terminal_context_target(binding, input_handler, window, cx) {
            return false;
        }
        self.set_reported_focus_session(Some(binding.session_id.clone()), cx);
        true
    }

    /// 使用 Application 的原子 owner/generation 校验全选，并只为仍绑定的只读 pane 刷新视图。
    ///
    /// @author kongweiguang
    fn start_select_all_for_binding(
        &mut self,
        binding: UiSessionBinding,
        source: Entity<TerminalToolsView>,
        cx: &mut Context<Self>,
    ) {
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let callback_binding = binding.clone();
        let selection_binding = binding.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(kerminal_application::AppError::cancelled("终端全选已取消"));
                    }
                    sessions.select_all_ui(binding).await?;
                    sessions.selected_text_ui(selection_binding).await
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                let still_registered = root
                    .terminal_view_bindings
                    .get(&callback_binding.session_id)
                    .is_some_and(|binding| binding == &callback_binding)
                    && root
                        .terminal_snapshot_for_binding(&callback_binding)
                        .is_some();
                if !still_registered {
                    return;
                }
                let Some(owner) = super::terminal_tools_runtime::resolve_owner(
                    root,
                    &source,
                    &callback_binding.session_id,
                    cx,
                ) else {
                    return;
                };
                if !super::terminal_tools_runtime::owner_is_current(root, &owner, cx) {
                    return;
                }
                if let Err(error) = &result {
                    if error.public_code() != kerminal_application::PublicErrorCode::Cancelled {
                        root.status = Some(error.public_message().into());
                        cx.notify();
                    }
                    return;
                }
                if let Ok(text) = &result {
                    root.terminal_tools_runtime.set_confirmed_selection_present(
                        &owner,
                        text.as_ref().is_some_and(|text| !text.is_empty()),
                    );
                }
                match &callback_binding.owner {
                    SessionOwner::Ui { tab_id, pane_id } => {
                        if root.workspace_runtime.pane(pane_id).is_some_and(|pane| {
                            pane.tab_id == *tab_id
                                && pane.session_id.as_ref() == Some(&callback_binding.session_id)
                        }) {
                            root.request_workspace_pane_snapshot(
                                tab_id.clone(),
                                pane_id.clone(),
                                cx,
                            );
                        }
                    }
                    SessionOwner::Agent { agent_session_id }
                        if root.agent_terminal_session_id.as_ref()
                            == Some(&callback_binding.session_id)
                            && root.selected_agent_session_id.as_ref()
                                == Some(agent_session_id) =>
                    {
                        root.request_agent_snapshot(cx);
                    }
                    SessionOwner::Agent { .. }
                    | SessionOwner::Mcp
                    | SessionOwner::Service { .. } => {}
                }
            });
        });
        task.detach();
    }

    /// 按 owner 类型激活原 pane 或 Agent；不可见/已切换目标不会借当前焦点替换 binding。
    ///
    /// @author kongweiguang
    fn focus_terminal_context_target(
        &mut self,
        binding: &UiSessionBinding,
        input_handler: Entity<PaneTerminalInputHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match &binding.owner {
            SessionOwner::Ui { tab_id, pane_id } => {
                self.focus_workspace_pane(tab_id.clone(), pane_id.clone(), window, cx);
            }
            SessionOwner::Agent { .. } => {
                self.focus_agent_terminal(input_handler, window, cx);
            }
            SessionOwner::Mcp | SessionOwner::Service { .. } => return false,
        }
        self.focused_terminal_session_id().as_ref() == Some(&binding.session_id)
    }
}

/// 清屏沿用旧版向终端发送 Ctrl+L 的行为，不伪装为本地删除 scrollback。
///
/// @author kongweiguang
pub(crate) const fn clear_screen_input_event() -> TerminalInputEvent {
    TerminalInputEvent::Key(TerminalKeyEvent {
        key: TerminalKey::Character('l'),
        phase: KeyPhase::Press,
        modifiers: InputModifiers {
            shift: false,
            alt: false,
            control: true,
            super_key: false,
        },
    })
}

/// 限定菜单实体仍对应选中的真实资源；切换 Tab、切换 Agent 或重连都会使旧闭包失效。
///
/// @author kongweiguang
fn terminal_context_target_is_current(
    root: &RootView,
    binding: &UiSessionBinding,
    input_handler: &Entity<PaneTerminalInputHandler>,
    tools: &Entity<TerminalToolsView>,
    cx: &App,
) -> bool {
    terminal_context_target_is_registered(root, binding, input_handler, tools, cx)
        && root.focused_terminal_session_id().as_ref() == Some(&binding.session_id)
}

/// Menu 只打开弹层；FocusOnly 与 Paste 仍遵循偏好明确要求把输入焦点交给命中终端。
///
/// @author kongweiguang
fn terminal_right_click_requires_focus(action: TerminalRightClickAction) -> bool {
    action != TerminalRightClickAction::Menu
}

/// 分屏上下文菜单按来源 pane 将四向布局或选择主机意图转为既有工作区事件。
///
/// @author kongweiguang
fn workspace_pane_event_for_menu_action(
    action: TerminalContextMenuAction,
    pane_id: kerminal_domain::identity::PaneId,
) -> Option<WorkspacePaneEvent> {
    match action {
        TerminalContextMenuAction::Split { direction, before } => Some(WorkspacePaneEvent::Split {
            pane_id,
            direction,
            before,
        }),
        TerminalContextMenuAction::ChooseSplitTarget { direction, before } => {
            Some(WorkspacePaneEvent::ChooseSplitTarget {
                pane_id,
                direction,
                before,
            })
        }
        _ => None,
    }
}

/// 只有会创建并随后接管输入的新分屏动作需要先锚定来源焦点；其它菜单动作沿原策略执行。
///
/// @author kongweiguang
fn split_menu_action_requires_terminal_focus(action: TerminalContextMenuAction) -> bool {
    matches!(
        action,
        TerminalContextMenuAction::Split { .. }
            | TerminalContextMenuAction::ChooseSplitTarget { .. }
    )
}

/// 菜单动作可作用于同一活动 tab 中的任意分屏；不得要求右键打开时先切换 pane 焦点。
///
/// @author kongweiguang
fn terminal_context_menu_target_is_active(
    root: &RootView,
    binding: &UiSessionBinding,
    input_handler: &Entity<PaneTerminalInputHandler>,
    tools: &Entity<TerminalToolsView>,
    cx: &App,
) -> bool {
    if !terminal_context_target_is_registered(root, binding, input_handler, tools, cx)
        || root.preferences_modal_open
        || root.connections_modal_open
        || root.connection_prompt_modal_open
        || root.snippet_editor_modal_open
        || root.root_confirmation_owner.is_some()
        || root.pending_quit_intent.is_some()
    {
        return false;
    }

    match &binding.owner {
        SessionOwner::Ui { tab_id, .. } => root
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| controller.snapshot().active_tab_id.as_ref() == Some(tab_id)),
        SessionOwner::Agent { agent_session_id } => {
            root.selected_agent_session_id.as_ref() == Some(agent_session_id)
        }
        SessionOwner::Mcp | SessionOwner::Service { .. } => false,
    }
}

/// 历史菜单只依赖 Root 保留的只读 view binding 与工具实体，不能把已退出输入句柄当权限凭据。
fn terminal_context_view_target_is_registered(
    root: &RootView,
    binding: &UiSessionBinding,
    tools: &Entity<TerminalToolsView>,
    cx: &App,
) -> bool {
    if root.terminal_view_bindings.get(&binding.session_id) != Some(binding)
        || root.terminal_snapshot_for_binding(binding).is_none()
        || tools.read(cx).session_id() != Some(&binding.session_id)
    {
        return false;
    }
    match &binding.owner {
        SessionOwner::Ui { .. } => root
            .workspace_tools
            .get(&binding.session_id)
            .is_some_and(|current| current.entity_id() == tools.entity_id()),
        SessionOwner::Agent { .. } => root
            .agent_terminal_tools
            .as_ref()
            .is_some_and(|current| current.entity_id() == tools.entity_id()),
        SessionOwner::Mcp | SessionOwner::Service { .. } => false,
    }
}

/// 菜单只在当前活动的逻辑目标且无 modal 时操作历史，切换目标后的旧弹层不越界。
fn terminal_context_view_target_is_active(
    root: &RootView,
    binding: &UiSessionBinding,
    tools: &Entity<TerminalToolsView>,
    cx: &App,
) -> bool {
    if !terminal_context_view_target_is_registered(root, binding, tools, cx)
        || root.preferences_modal_open
        || root.connections_modal_open
        || root.connection_prompt_modal_open
        || root.snippet_editor_modal_open
        || root.root_confirmation_owner.is_some()
        || root.pending_quit_intent.is_some()
    {
        return false;
    }
    match &binding.owner {
        SessionOwner::Ui { tab_id, .. } => root
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| controller.snapshot().active_tab_id.as_ref() == Some(tab_id)),
        SessionOwner::Agent { agent_session_id } => {
            root.selected_agent_session_id.as_ref() == Some(agent_session_id)
        }
        SessionOwner::Mcp | SessionOwner::Service { .. } => false,
    }
}

/// 右键发生在未激活的分屏时允许先激活其精确资源；这里不要求该 pane 已是键盘焦点。
///
/// @author kongweiguang
fn terminal_context_target_is_registered(
    root: &RootView,
    binding: &UiSessionBinding,
    input_handler: &Entity<PaneTerminalInputHandler>,
    tools: &Entity<TerminalToolsView>,
    cx: &App,
) -> bool {
    if !binding_matches_exactly(binding, input_handler.read(cx).binding())
        || tools.read(cx).session_id() != Some(&binding.session_id)
        || !root
            .pane_input_handlers
            .get(&binding.session_id)
            .is_some_and(|registration| {
                binding_matches_exactly(binding, &registration.binding)
                    && registration.entity.entity_id() == input_handler.entity_id()
            })
    {
        return false;
    }

    match &binding.owner {
        SessionOwner::Ui { tab_id, pane_id } => {
            root.center_panel == super::CenterPanel::Terminal
                && root.workspace_runtime.pane(pane_id).is_some_and(|pane| {
                    pane.tab_id == *tab_id
                        && pane.session_id.as_ref() == Some(&binding.session_id)
                        && pane.phase == RuntimePanePhase::Ready
                })
                && root.workspace_runtime.session_for_pane(pane_id) == Some(&binding.session_id)
                && root
                    .workspace_tools
                    .get(&binding.session_id)
                    .is_some_and(|current| current.entity_id() == tools.entity_id())
        }
        SessionOwner::Agent { agent_session_id } => {
            root.selected_agent_session_id.as_ref() == Some(agent_session_id)
                && root.agent_terminal_session_id.as_ref() == Some(&binding.session_id)
                && root
                    .agent_terminal_tools
                    .as_ref()
                    .is_some_and(|current| current.entity_id() == tools.entity_id())
        }
        SessionOwner::Mcp | SessionOwner::Service { .. } => false,
    }
}

/// 绑定比较必须包含 generation，避免重连后旧菜单闭包被当作相同 session 使用。
///
/// @author kongweiguang
fn binding_matches_exactly(expected: &UiSessionBinding, actual: &UiSessionBinding) -> bool {
    expected == actual
}

#[cfg(test)]
#[path = "../../tests/unit/root_terminal_context_menu.rs"]
mod tests;
