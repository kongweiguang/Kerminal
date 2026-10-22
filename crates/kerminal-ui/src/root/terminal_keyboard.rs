//! 终端画布键盘输入与快捷键路由；粘贴和按键共用固定 pane binding。
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity, KeyDownEvent, Window};
use kerminal_domain::{commands::SuggestionQueryMode, sessions::SessionOwner};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyAlternates,
    TerminalKeyEvent, encode_input,
};
use kerminal_terminal::snapshot::TerminalModes;

use crate::features::terminal_tools::TerminalToolsEvent;

use super::{
    RootView, SessionCommand, UiSessionBinding, command_suggestions, keybinding_focus, keybindings,
    pane_input::PaneTerminalInputHandler, terminal_input_modal_blocked, terminal_key,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalClipboardAction {
    CopySelection,
    Paste,
    Interrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalViewportScroll {
    Relative(i32),
    Absolute(usize),
}

/// 将滚动快捷键与剪贴板手势集中解析，避免按键分流和终端协议编码各自解释修饰键。
/// Kitty 已声明接管按键时保留原始事件；备用屏上的按键也继续交给 TUI。
///
/// @author kongweiguang
fn viewport_scroll_action(
    key: TerminalKey,
    modifiers: InputModifiers,
    modes: TerminalModes,
    page_rows: usize,
) -> Option<TerminalViewportScroll> {
    if modes.alternate_screen || modes.kitty_keyboard_flags != 0 {
        return None;
    }

    viewport_scroll_action_for_keys(key, modifiers, page_rows)
}

/// 解析纯本地视口快捷键；调用方已确认输入归属，避免历史 pane 受退出前残留 TUI 模式阻挡。
///
/// @author kongweiguang
fn viewport_scroll_action_for_keys(
    key: TerminalKey,
    modifiers: InputModifiers,
    page_rows: usize,
) -> Option<TerminalViewportScroll> {
    let plain_shift =
        modifiers.shift && !modifiers.control && !modifiers.alt && !modifiers.super_key;
    let control_shift =
        modifiers.control && modifiers.shift && !modifiers.alt && !modifiers.super_key;
    match (key, plain_shift, control_shift) {
        (TerminalKey::PageUp, true, _) => Some(TerminalViewportScroll::Relative(
            i32::try_from(page_rows.max(1)).unwrap_or(i32::MAX),
        )),
        (TerminalKey::PageDown, true, _) => Some(TerminalViewportScroll::Relative(
            -i32::try_from(page_rows.max(1)).unwrap_or(i32::MAX),
        )),
        (TerminalKey::Home, _, true) => Some(TerminalViewportScroll::Absolute(usize::MAX)),
        (TerminalKey::End, _, true) => Some(TerminalViewportScroll::Absolute(0)),
        _ => None,
    }
}

/// 依据固定复制约定区分“有选择时复制”和无选择时发出终端中断。
/// Cmd+C 继续沿用 macOS 复制习惯；Alt 组合不参与剪贴板快捷键。
///
/// @author kongweiguang
fn terminal_clipboard_action(
    key: TerminalKey,
    modifiers: InputModifiers,
    has_selection: bool,
) -> Option<TerminalClipboardAction> {
    if modifiers.alt {
        return None;
    }
    let character = match key {
        TerminalKey::Character(character) => character.to_ascii_lowercase(),
        _ => return None,
    };
    let command = modifiers.super_key;
    if character == 'c' && (command || (modifiers.control && modifiers.shift)) {
        return Some(TerminalClipboardAction::CopySelection);
    }
    if character == 'c' && modifiers.control && !modifiers.shift && !command {
        return Some(if has_selection {
            TerminalClipboardAction::CopySelection
        } else {
            TerminalClipboardAction::Interrupt
        });
    }
    if character == 'v' && (command || (modifiers.control && modifiers.shift)) {
        return Some(TerminalClipboardAction::Paste);
    }
    None
}

/// 复用已聚焦 pane 的固定 binding 和输入队列投递剪贴板内容，不另建异步输入通道。
///
/// @author kongweiguang
pub(super) fn paste_bound_terminal(
    root: &mut RootView,
    binding: &UiSessionBinding,
    input_handler: Entity<PaneTerminalInputHandler>,
    cx: &mut Context<RootView>,
) -> bool {
    let handler_matches = input_handler.read(cx).binding() == binding
        && root
            .pane_input_handlers
            .get(&binding.session_id)
            .is_some_and(|registration| {
                registration.binding == *binding
                    && registration.entity.entity_id() == input_handler.entity_id()
            });
    if !handler_matches || root.logical_focused_pane_binding(cx).as_ref() != Some(binding) {
        return false;
    }

    let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
        return true;
    };
    if text.is_empty() {
        return true;
    }

    let accepted = input_handler.update(cx, |handler, cx| {
        if handler.binding() != binding {
            return false;
        }
        handler.emit_input_event(TerminalInputEvent::Paste(text), cx)
    });
    if !accepted {
        root.status = Some(crate::locale::text(
            "当前终端已不可用，剪贴板内容未能粘贴。",
            "The focused terminal is unavailable; clipboard text was not pasted.",
        ));
        cx.notify();
    }
    accepted
}

impl RootView {
    /// 只在 WorkspaceTabs 确认自己持有 Tab/Group drag 时取消该 GPUI drag 与边缘滚动，不影响其它组件手势。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_workspace_tab_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.workspace_tabs_view.update(cx, |tabs, tabs_cx| {
            tabs.cancel_workspace_tab_drag(window, tabs_cx)
        })
    }

    /// 先处理工作区拖动取消，再由 Quick Open 镜像门禁阻止迟到终端事件，随后处理本地手势和协议按键。
    /// 普通字符交还平台文本/IME；Windows 会用传播结果决定是否执行 TranslateMessage。
    ///
    /// @author kongweiguang
    pub(super) fn handle_pane_key_down(
        &mut self,
        input_handler: Entity<PaneTerminalInputHandler>,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ime_composing = input_handler.read(cx).is_composing();
        if !ime_composing
            && !event.is_held
            && event.keystroke.key.eq_ignore_ascii_case("escape")
            && self.cancel_workspace_tab_drag(window, cx)
        {
            self.keybinding_runtime.clear_pending();
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if terminal_input_modal_blocked(false, self.quick_open_modal_open) {
            // Quick Open 可见时，即使系统仍把按键送给旧终端 Entity，也不能落入 PTY。
            cx.stop_propagation();
            return;
        }
        let binding = input_handler.read(cx).binding().clone();
        if self
            .pending_pane_focus_intent
            .as_ref()
            .is_some_and(|intent| {
                !matches!(
                    &binding.owner,
                    SessionOwner::Ui { pane_id, .. } if pane_id == &intent.pane_id
                )
            })
        {
            self.clear_pending_pane_focus();
        }
        self.observe_keybinding_focus(window, cx);
        if !self
            .pane_input_handlers
            .get(&binding.session_id)
            .is_some_and(|registered| registered.binding == binding)
        {
            cx.stop_propagation();
            return;
        }
        if !self.pane_input_binding_is_focused(&binding, window, cx) {
            cx.stop_propagation();
            return;
        }
        input_handler.update(cx, |handler, _| {
            handler.note_plain_keydown_after_focus_loss(None);
        });
        if keybinding_focus::pane_key_is_owned_by_ime(ime_composing) {
            self.suspend_suggestions_for_ime(&binding, cx);
            return;
        }
        let modifiers = self.terminal_input_modifiers(event.keystroke.modifiers);
        let terminal_event = terminal_key(&event.keystroke.key)
            .or_else(|| {
                (modifiers.control || modifiers.alt)
                    .then(|| event.keystroke.key.chars().next())
                    .flatten()
                    .map(TerminalKey::Character)
            })
            .map(|key| TerminalKeyEvent {
                key,
                phase: if event.is_held {
                    KeyPhase::Repeat
                } else {
                    KeyPhase::Press
                },
                modifiers,
            });

        let (modes, page_rows, snapshot_has_selection) = self
            .snapshot_for_session(&binding.session_id)
            .map(|snapshot| {
                (
                    snapshot.modes,
                    snapshot.rows.len().max(1),
                    snapshot.selection.is_some(),
                )
            })
            .unwrap_or((TerminalModes::default(), 1, false));
        let selection_tools = match &binding.owner {
            SessionOwner::Ui { .. } => self.workspace_tools.get(&binding.session_id),
            SessionOwner::Agent { .. } => self.agent_terminal_tools.as_ref(),
            SessionOwner::Mcp | SessionOwner::Service { .. } => None,
        };
        let selection_gesture_active = selection_tools
            .is_some_and(|tools| tools.read(cx).has_active_selection_gesture(&binding));
        let selection_update_pending = selection_tools
            .and_then(|tools| {
                super::terminal_tools_runtime::resolve_owner(self, tools, &binding.session_id, cx)
            })
            .is_some_and(|owner| self.terminal_tools_runtime.selection_may_be_present(&owner));
        let has_selection =
            snapshot_has_selection || selection_gesture_active || selection_update_pending;

        if self.suggestion_menu_binding.as_ref() == Some(&binding)
            && self.terminal_command_suggestions.menu_is_open()
            && let Some(menu) = self.suggestion_menu.as_ref()
        {
            let menu_key = terminal_event.as_ref().and_then(|event| match event.key {
                TerminalKey::Up => Some(command_suggestions::TerminalCommandSuggestionMenuKey::Up),
                TerminalKey::Down => {
                    Some(command_suggestions::TerminalCommandSuggestionMenuKey::Down)
                }
                TerminalKey::Enter => {
                    Some(command_suggestions::TerminalCommandSuggestionMenuKey::Enter)
                }
                TerminalKey::Escape => {
                    Some(command_suggestions::TerminalCommandSuggestionMenuKey::Escape)
                }
                _ => None,
            });
            if let Some(menu_key) = menu_key {
                self.keybinding_runtime.clear_pending();
                menu.update(cx, |menu, cx| menu.handle_key(menu_key, cx));
                cx.stop_propagation();
                return;
            }
        }

        let suggestion_context = self.suggestion_context_for_binding(&binding);
        if let (Some(context), Some(terminal_event)) =
            (suggestion_context.as_ref(), terminal_event.as_ref())
        {
            if let Some(job) = self
                .terminal_command_suggestions
                .request_menu_on_key(*terminal_event, context)
            {
                self.keybinding_runtime.clear_pending();
                self.suggestion_menu_binding = Some(binding.clone());
                self.sync_suggestion_menu(cx);
                self.run_terminal_suggestion_job(job, cx);
                cx.stop_propagation();
                return;
            }
            if let Some(acceptance) = self
                .terminal_command_suggestions
                .accept_on_key(*terminal_event, context)
            {
                let mode = if self.terminal_command_suggestions.menu_is_open() {
                    SuggestionQueryMode::Menu
                } else {
                    SuggestionQueryMode::Inline
                };
                if self.enqueue_suggestion_acceptance(
                    binding.clone(),
                    context,
                    acceptance,
                    mode,
                    cx,
                ) {
                    self.keybinding_runtime.clear_pending();
                    cx.stop_propagation();
                    return;
                }
            }
        }

        if terminal_event.is_some_and(|terminal_event| {
            terminal_event.key == TerminalKey::Escape && terminal_event.phase == KeyPhase::Press
        }) && (self.close_terminal_search_for_binding(&binding, window, cx)
            || dispatch_selection_action(
                self,
                &binding,
                TerminalToolsEvent::ClearSelection {
                    session_id: binding.session_id.clone(),
                },
                has_selection,
                window,
                cx,
            ))
        {
            self.keybinding_runtime.clear_pending();
            cx.stop_propagation();
            return;
        }

        if let Some(action) = terminal_event.and_then(|terminal_event| {
            terminal_clipboard_action(terminal_event.key, modifiers, has_selection)
        }) {
            if event.is_held && action != TerminalClipboardAction::Interrupt {
                self.keybinding_runtime.clear_pending();
                cx.stop_propagation();
                return;
            }
            match action {
                TerminalClipboardAction::CopySelection => {
                    if !dispatch_selection_action(
                        self,
                        &binding,
                        TerminalToolsEvent::CopySelection {
                            session_id: binding.session_id.clone(),
                        },
                        has_selection || modifiers.shift || modifiers.super_key,
                        window,
                        cx,
                    ) {
                        self.status = Some(crate::locale::text(
                            "当前没有可复制的终端选择。",
                            "There is no terminal selection to copy.",
                        ));
                        cx.notify();
                    }
                }
                TerminalClipboardAction::Paste => {
                    if !paste_bound_terminal(self, &binding, input_handler.clone(), cx) {
                        self.status = Some(crate::locale::text(
                            "当前终端不可用，剪贴板内容未能粘贴。",
                            "The focused terminal is unavailable; clipboard text was not pasted.",
                        ));
                        cx.notify();
                    }
                }
                TerminalClipboardAction::Interrupt => {
                    if let Some(key_event) = terminal_event
                        && let Some(input) = terminal_key_input(key_event, modes, None)
                    {
                        self.set_reported_focus_session(Some(binding.session_id.clone()), cx);
                        let _ = input_handler
                            .update(cx, |handler, cx| handler.emit_input_event(input, cx));
                    }
                }
            }
            self.keybinding_runtime.clear_pending();
            cx.stop_propagation();
            return;
        }

        match self.keybinding_runtime.dispatch(
            &event.keystroke,
            keybindings::KeybindingFocusScope::Terminal,
            ime_composing,
            false,
            event.is_held,
        ) {
            keybindings::KeybindingDispatch::Unhandled => {}
            keybindings::KeybindingDispatch::Pending => {
                cx.stop_propagation();
                return;
            }
            keybindings::KeybindingDispatch::Action(action) => {
                if action == crate::keybindings::KeybindingAction::TerminalPaste {
                    let _ = paste_bound_terminal(self, &binding, input_handler.clone(), cx);
                    cx.stop_propagation();
                    return;
                }
                if action == crate::keybindings::KeybindingAction::TerminalSearch {
                    self.keybinding_runtime.clear_pending();
                    if !self.open_terminal_search_for_binding(&binding, window, cx) {
                        self.status = Some(crate::locale::text(
                            "当前终端搜索面板不可用。",
                            "Terminal search is unavailable for this target.",
                        ));
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                if self.dispatch_keybinding_action(action, window, cx) {
                    cx.stop_propagation();
                    return;
                }
            }
        }

        if let Some(scroll) = terminal_event.and_then(|terminal_event| {
            viewport_scroll_action(terminal_event.key, modifiers, modes, page_rows)
        }) && dispatch_viewport_scroll(self, &binding, scroll, cx)
        {
            self.keybinding_runtime.clear_pending();
            cx.stop_propagation();
            return;
        }

        let session_id = binding.session_id.clone();
        let plain_text = (!modifiers.control && !modifiers.alt && !modifiers.super_key)
            .then_some(event.keystroke.key_char.as_ref())
            .flatten()
            .filter(|text| !text.is_empty())
            .map(|text| text.as_ref());
        input_handler.update(cx, |handler, _| {
            handler.note_plain_keydown_after_focus_loss(plain_text);
        });
        let Some(key_event) = terminal_event else {
            return;
        };
        let associated_text = input_handler
            .read(cx)
            .can_send_kitty_associated_text()
            .then_some(event.keystroke.key_char.as_ref())
            .flatten()
            .filter(|text| !text.is_empty())
            .cloned();
        let Some(input) = terminal_key_input(key_event, modes, associated_text.as_deref()) else {
            // 普通字符需要继续产生平台 commit；既不发送空输入，也不抑制后续文本。
            return;
        };
        if let TerminalInputEvent::KeyWithText { text, .. } = &input {
            self.remember_key_commit(session_id.clone(), text.clone(), cx);
        }
        self.set_reported_focus_session(Some(session_id), cx);
        let _ = input_handler.update(cx, |handler, cx| handler.emit_input_event(input, cx));
        cx.stop_propagation();
    }

    /// Exited pane 仅保留读操作；本地滚动不读取旧 TUI 模式，且任何事件都不向已退出 PTY 写入。
    ///
    /// @author kongweiguang
    pub(super) fn handle_terminal_history_key_down(
        &mut self,
        binding: UiSessionBinding,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.terminal_view_bindings.get(&binding.session_id) != Some(&binding) {
            return false;
        }
        let Some(snapshot) = self.terminal_snapshot_for_binding(&binding).cloned() else {
            return false;
        };
        let tools = match &binding.owner {
            SessionOwner::Ui { .. } => self.workspace_tools.get(&binding.session_id).cloned(),
            SessionOwner::Agent { .. } => self.agent_terminal_tools.clone(),
            SessionOwner::Mcp | SessionOwner::Service { .. } => None,
        };
        let active_gesture = tools
            .as_ref()
            .is_some_and(|tools| tools.read(cx).has_active_selection_gesture(&binding));
        let selection_update_pending = tools
            .as_ref()
            .and_then(|tools| {
                super::terminal_tools_runtime::resolve_owner(self, tools, &binding.session_id, cx)
            })
            .is_some_and(|owner| self.terminal_tools_runtime.selection_may_be_present(&owner));
        let has_selection =
            snapshot.selection.is_some() || active_gesture || selection_update_pending;
        let modifiers = self.terminal_input_modifiers(event.keystroke.modifiers);
        let terminal_event = terminal_key(&event.keystroke.key)
            .or_else(|| {
                (modifiers.control || modifiers.alt)
                    .then(|| event.keystroke.key.chars().next())
                    .flatten()
                    .map(TerminalKey::Character)
            })
            .map(|key| TerminalKeyEvent {
                key,
                phase: if event.is_held {
                    KeyPhase::Repeat
                } else {
                    KeyPhase::Press
                },
                modifiers,
            });

        if terminal_event
            .is_some_and(|key| key.key == TerminalKey::Escape && key.phase == KeyPhase::Press)
        {
            if self.close_terminal_search_for_binding(&binding, window, cx) {
                self.keybinding_runtime.clear_pending();
                return true;
            }
            if has_selection && let Some(tools) = tools {
                tools.update(cx, |tools, _| tools.cancel_viewport_selection_drag());
                return dispatch_selection_action(
                    self,
                    &binding,
                    TerminalToolsEvent::ClearSelection {
                        session_id: binding.session_id.clone(),
                    },
                    true,
                    window,
                    cx,
                );
            }
            return false;
        }

        if let Some(action) = terminal_event
            .and_then(|key| terminal_clipboard_action(key.key, modifiers, has_selection))
        {
            if event.is_held && action != TerminalClipboardAction::Interrupt {
                self.keybinding_runtime.clear_pending();
                return true;
            }
            match action {
                TerminalClipboardAction::CopySelection => {
                    if !dispatch_selection_action(
                        self,
                        &binding,
                        TerminalToolsEvent::CopySelection {
                            session_id: binding.session_id.clone(),
                        },
                        has_selection,
                        window,
                        cx,
                    ) {
                        self.status = Some(crate::locale::text(
                            "当前没有可复制的终端选择。",
                            "There is no terminal selection to copy.",
                        ));
                        cx.notify();
                    }
                }
                TerminalClipboardAction::Interrupt => {
                    self.status = Some(crate::locale::text(
                        "终端已退出，历史内容只读，无法发送 Ctrl+C。",
                        "The terminal has exited; its history is read-only and cannot receive Ctrl+C.",
                    ));
                    cx.notify();
                }
                TerminalClipboardAction::Paste => {
                    self.status = Some(crate::locale::text(
                        "终端已退出，历史内容只读，无法粘贴。",
                        "The terminal has exited; its history is read-only and cannot receive paste input.",
                    ));
                    cx.notify();
                }
            }
            self.keybinding_runtime.clear_pending();
            return true;
        }

        if let Some(key) = terminal_event
            && let Some(scroll) =
                viewport_scroll_action_for_keys(key.key, modifiers, snapshot.rows.len().max(1))
        {
            self.keybinding_runtime.clear_pending();
            let _ = dispatch_viewport_scroll(self, &binding, scroll, cx);
            return true;
        }

        match self.keybinding_runtime.dispatch(
            &event.keystroke,
            keybindings::KeybindingFocusScope::Terminal,
            false,
            false,
            event.is_held,
        ) {
            keybindings::KeybindingDispatch::Unhandled => false,
            keybindings::KeybindingDispatch::Pending => true,
            keybindings::KeybindingDispatch::Action(action) => {
                self.keybinding_runtime.clear_pending();
                match action {
                    crate::keybindings::KeybindingAction::TerminalSearch => {
                        self.open_terminal_search_for_binding(&binding, window, cx)
                    }
                    crate::keybindings::KeybindingAction::TerminalPaste => {
                        self.status = Some(crate::locale::text(
                            "终端已退出，历史内容只读，无法粘贴。",
                            "The terminal has exited; its history is read-only and cannot receive paste input.",
                        ));
                        cx.notify();
                        true
                    }
                    _ => false,
                }
            }
        }
    }
}

/// 将 PgUp/PgDn 作为相对命令排队，避免多个未确认按键共享过期绝对 offset。
fn dispatch_viewport_scroll(
    root: &mut RootView,
    binding: &UiSessionBinding,
    scroll: TerminalViewportScroll,
    cx: &mut Context<RootView>,
) -> bool {
    match scroll {
        TerminalViewportScroll::Relative(lines) => root.enqueue_session_command(
            SessionCommand::Scroll {
                binding: binding.clone(),
                lines,
            },
            cx,
        ),
        TerminalViewportScroll::Absolute(offset) => {
            root.handle_terminal_viewport_scroll(binding.clone(), offset, cx)
        }
    }
}

/// 选择动作始终发给 binding 所属的工具实体，失去选择或工具尚未装配时不转投当前焦点。
///
/// @author kongweiguang
fn dispatch_selection_action(
    root: &mut RootView,
    binding: &UiSessionBinding,
    event: TerminalToolsEvent,
    has_selection: bool,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    if !has_selection
        || root.terminal_view_bindings.get(&binding.session_id) != Some(binding)
        || root.terminal_snapshot_for_binding(binding).is_none()
    {
        return false;
    }
    let tools = match &binding.owner {
        SessionOwner::Ui { .. } => root.workspace_tools.get(&binding.session_id).cloned(),
        SessionOwner::Agent { .. } => root.agent_terminal_tools.clone(),
        SessionOwner::Mcp | SessionOwner::Service { .. } => None,
    };
    let Some(tools) = tools else {
        root.status = Some(crate::locale::text(
            "当前终端选择工具尚未就绪。",
            "Selection tools for the focused terminal are not ready.",
        ));
        cx.notify();
        return true;
    };
    root.handle_terminal_tools_event_from(tools, &event, window, cx);
    true
}

/// 复用唯一协议编码器判定本次 keydown 是否被接管，不在 UI 再维护字符/模式白名单。
///
/// 普通字母在 legacy 模式返回 None，由 GPUI 的文本回调提交；Kitty 等实际编码的按键仍沿原 FIFO。
/// 编码结果只用于分流，真实写入仍在原 owner/generation 校验后的运行时执行，不能直接绕写 PTY。
///
/// @author kongweiguang
fn terminal_key_input(
    key: TerminalKeyEvent,
    modes: TerminalModes,
    associated_text: Option<&str>,
) -> Option<TerminalInputEvent> {
    let input = match (
        modes.kitty_keyboard_flags & 0b1_1000 == 0b1_1000,
        associated_text,
    ) {
        (true, Some(text)) => TerminalInputEvent::KeyWithText {
            event: key,
            text: text.to_owned(),
            alternates: TerminalKeyAlternates::default(),
        },
        _ => TerminalInputEvent::Key(key),
    };
    encode_input(&input, modes).map(|_| input)
}

/// 只有终端协议确实会编码的 key-up 才进入输入 FIFO，避免空释放事件被误认为真实输入。
/// Kitty `REPORT_EVENT_TYPES` 下仍保留可编码的释放通知；legacy 模式不产生空输入副作用。
///
/// @author kongweiguang
pub(super) fn terminal_key_release_input(
    key: TerminalKey,
    modifiers: InputModifiers,
    modes: TerminalModes,
) -> Option<TerminalInputEvent> {
    terminal_key_input(
        TerminalKeyEvent {
            key,
            phase: KeyPhase::Release,
            modifiers,
        },
        modes,
        None,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/root_terminal_keyboard.rs"]
mod tests;
