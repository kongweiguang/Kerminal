//! 终端画布键盘输入与快捷键路由；粘贴和按键共用固定 pane binding。
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity, KeyDownEvent, Modifiers, Window};
use kerminal_domain::{commands::SuggestionQueryMode, sessions::SessionOwner};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyAlternates,
    TerminalKeyEvent, encode_input,
};
use kerminal_terminal::snapshot::TerminalModes;

use crate::{features::terminal_tools::TerminalToolsEvent, keybindings::KeybindingAction};

use super::{
    RootView, SessionCommand, UiSessionBinding, command_suggestions, keybinding_focus, keybindings,
    pane_input::PaneTerminalInputHandler, terminal_input_modal_blocked, terminal_key,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalClipboardAction {
    CopySelection,
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

/// 只按物理修饰键判定复制手势；平台参数显式传入，让跨平台单测与宿主系统解耦。
///
/// 复制组合必须是精确集合：macOS 仅 Cmd+C，Windows/Linux 额外支持 Ctrl+Insert，
/// Ctrl+C/Ctrl+Shift+C 保持既有“有选择复制、无选择中断”的语义。Alt（AltGr、物理
/// Option）和 Fn 一律排除，Win/Ctrl/Shift 混合同样不触发，避免与系统或 TUI 快捷键冲突。
///
/// @author kongweiguang
fn terminal_clipboard_action_for_platform(
    key: TerminalKey,
    modifiers: Modifiers,
    has_selection: bool,
    macos: bool,
) -> Option<TerminalClipboardAction> {
    if modifiers.alt || modifiers.function {
        return None;
    }
    match key {
        TerminalKey::Character(character) => {
            if !character.eq_ignore_ascii_case(&'c') {
                return None;
            }
            if macos && modifiers.platform && !modifiers.control && !modifiers.shift {
                return Some(TerminalClipboardAction::CopySelection);
            }
            if modifiers.control && !modifiers.platform {
                return Some(if modifiers.shift || has_selection {
                    TerminalClipboardAction::CopySelection
                } else {
                    TerminalClipboardAction::Interrupt
                });
            }
            None
        }
        // Zed 与 Windows Terminal 在 Windows/Linux 用 Ctrl+Insert 复制；macOS 的
        // Insert 组合不是复制习惯，仍交给终端协议编码。
        TerminalKey::Insert
            if !macos && modifiers.control && !modifiers.shift && !modifiers.platform =>
        {
            Some(TerminalClipboardAction::CopySelection)
        }
        _ => None,
    }
}

/// 本地复制手势先服从平台字符输入优先级，再使用原始 GPUI 修饰键判定。
///
/// 不能复用终端输入修饰键：macOS 关闭 Option-as-Meta 后映射会抹掉物理 Option，
/// 若再用映射结果判断，Cmd+Option+C 会被误认成 Cmd+C。IME 组合或 AltGr 声明
/// 字符优先时也必须原样交还平台文本通道。
///
/// @author kongweiguang
fn terminal_clipboard_shortcut(
    key: TerminalKey,
    event: &KeyDownEvent,
    has_selection: bool,
    ime_composing: bool,
) -> Option<TerminalClipboardAction> {
    if ime_composing || event.prefer_character_input {
        return None;
    }
    terminal_clipboard_action_for_platform(
        key,
        event.keystroke.modifiers,
        has_selection,
        cfg!(target_os = "macos"),
    )
}

/// Ctrl+Insert 在 Windows/Linux 既是未配置时的默认复制手势，也可能被用户显式绑定为动作。
/// 在剪贴板分支前只对精确的物理 Ctrl+Insert 先跑一次同一 keymap resolver，
/// 命中动作、插件或 chord 前缀时让配置优先；未命中（Unhandled）才回落到本地复制。
/// 这里返回同一次解析结果供后续分支复用，避免二次 dispatch 改变多段序列的前缀状态。
///
/// @author kongweiguang
fn resolve_ctrl_insert_override(
    runtime: &mut keybindings::RootKeybindingRuntime,
    key: TerminalKey,
    event: &KeyDownEvent,
    ime_composing: bool,
) -> Option<keybindings::KeybindingDispatch> {
    // macOS 的 Insert 组合不是复制习惯，也不参与该优先级，仍交给终端协议编码。
    if cfg!(target_os = "macos")
        || key != TerminalKey::Insert
        || ime_composing
        || event.prefer_character_input
    {
        return None;
    }
    let modifiers = event.keystroke.modifiers;
    // 与本地复制保持同一精确组合：Shift/Alt/Win/Fn 混合键不能被当成可配置 Ctrl+Insert。
    if !modifiers.control
        || modifiers.shift
        || modifiers.alt
        || modifiers.platform
        || modifiers.function
    {
        return None;
    }
    Some(resolve_terminal_shortcut(runtime, event, ime_composing))
}

/// 只有配置 keymap 真正命中的 Ctrl+Insert 才压过默认复制；Unhandled 表示没有用户绑定。
///
/// @author kongweiguang
fn ctrl_insert_override_wins(dispatch: Option<&keybindings::KeybindingDispatch>) -> bool {
    dispatch.is_some_and(|dispatch| *dispatch != keybindings::KeybindingDispatch::Unhandled)
}

/// 判定某次 keydown 是否只是已消费快捷键的长按重复。
///
/// 只看物理主键，因此修饰键先松开仍能识别；`prefer_character_input` 时保留文本通道，
/// 并让 `resolve_terminal_shortcut` 负责清理旧记录。
///
/// @author kongweiguang
fn consumed_terminal_shortcut_repeat(
    runtime: &keybindings::RootKeybindingRuntime,
    event: &KeyDownEvent,
) -> bool {
    !event.prefer_character_input
        && event.is_held
        && runtime.is_terminal_shortcut_repeat(&event.keystroke)
}

/// AltGr 的布局字符由平台文本回调提交，避免把同一个键按 Ctrl+Alt 编成控制字节。
/// 普通 Ctrl+Alt 快捷键可能也有优先标记，只有可打印的布局字符才切换到文本路径。
///
/// @author kongweiguang
fn platform_preferred_terminal_text(event: &KeyDownEvent) -> Option<&str> {
    event
        .prefer_character_input
        .then_some(event.keystroke.key_char.as_deref())
        .flatten()
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
}

/// 尊重平台标记的字符输入优先级，避免 AltGr 或 IME 按键被用户快捷键误消费。
///
/// @author kongweiguang
fn resolve_terminal_shortcut(
    runtime: &mut keybindings::RootKeybindingRuntime,
    event: &KeyDownEvent,
    ime_composing: bool,
) -> keybindings::KeybindingDispatch {
    if ime_composing || event.prefer_character_input {
        runtime.clear_pending();
        runtime.clear_terminal_shortcut_keys();
        return keybindings::KeybindingDispatch::Unhandled;
    }
    if event.is_held && runtime.is_terminal_shortcut_repeat(&event.keystroke) {
        return keybindings::KeybindingDispatch::Pending;
    }
    runtime.dispatch(
        &event.keystroke,
        keybindings::KeybindingFocusScope::Terminal,
        ime_composing,
        false,
        event.is_held,
    )
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

/// Agent 终端没有中央标签或分屏身份，不能用当前显示的工作区代替按键目标。
fn terminal_action_can_target_workspace(action: KeybindingAction, owner: &SessionOwner) -> bool {
    !matches!(owner, SessionOwner::Agent { .. })
        || !matches!(
            action,
            KeybindingAction::TerminalCloseTab
                | KeybindingAction::TerminalClosePane
                | KeybindingAction::TerminalSplitHorizontal
                | KeybindingAction::TerminalSplitVertical
        )
}

impl RootView {
    /// 本地操作可能立即改变界面，仍须保留本次按键的归属，避免长按或 Kitty 释放落入 Shell。
    ///
    /// @author kongweiguang
    fn consume_local_terminal_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.keybinding_runtime.clear_pending();
        self.keybinding_runtime
            .remember_terminal_shortcut_key(&event.keystroke);
        window.prevent_default();
        cx.stop_propagation();
    }

    /// 先取消左栏主机拖动，再处理 WorkspaceTabs；组件确认归属后才停止 GPUI drag。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_workbench_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.host_navigation.as_ref().is_some_and(|navigation| {
            navigation.update(cx, |navigation, navigation_cx| {
                navigation.cancel_host_drag(window, navigation_cx)
            })
        }) {
            return true;
        }
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
        if let Some(text) = platform_preferred_terminal_text(event) {
            self.keybinding_runtime.clear_pending();
            self.keybinding_runtime
                .remember_terminal_shortcut_key(&event.keystroke);
            input_handler.update(cx, |handler, _| {
                handler.note_plain_keydown_after_focus_loss(Some(text));
            });
            return;
        }
        let plain_escape_press = !event.is_held
            && !event.prefer_character_input
            && !event.keystroke.modifiers.modified()
            && event.keystroke.key.eq_ignore_ascii_case("escape");
        if plain_escape_press && self.cancel_workbench_drag(window, cx) {
            self.consume_local_terminal_key(event, window, cx);
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
        if consumed_terminal_shortcut_repeat(&self.keybinding_runtime, event) {
            // 已消费的物理主键在 keyup 前保持同一身份：模式仍允许时继续本地滚动以保留
            // 连续翻页；否则整段吞掉，避免选择消失后 Ctrl+C 重算为 Interrupt，或
            // Kitty/alternate 状态切换、修饰键先松开时把重复键漏进 PTY。
            if self
                .keybinding_runtime
                .is_terminal_scroll_key(&event.keystroke)
                && let Some(scroll) = terminal_event.and_then(|terminal_event| {
                    viewport_scroll_action(terminal_event.key, modifiers, modes, page_rows)
                })
            {
                let _ = dispatch_viewport_scroll(self, &binding, scroll, cx);
            }
            // 重复按键不是新 stroke，保留尚未超时的组合键前缀。
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
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
        let suggestion_anchor_available = self
            .snapshot_for_session(&binding.session_id)
            .is_some_and(|snapshot| {
                super::suggestion_overlay::anchor_available(snapshot, has_selection)
            });
        let native_prediction_visible = self
            .snapshot_for_session(&binding.session_id)
            .is_some_and(super::suggestion_overlay::has_native_prediction);

        if !suggestion_anchor_available
            && self.suggestion_menu_binding.as_ref() == Some(&binding)
            && self.terminal_command_suggestions.menu_is_open()
        {
            self.terminal_command_suggestions.dismiss_menu();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
            // 菜单仍可见时按下 Enter，意图是接受建议。锚点失效后也不能让这次按键执行命令。
            if !event.prefer_character_input
                && !event.keystroke.modifiers.modified()
                && event.keystroke.key.eq_ignore_ascii_case("enter")
            {
                self.consume_local_terminal_key(event, window, cx);
                return;
            }
        }

        if suggestion_anchor_available
            && !event.prefer_character_input
            && self.suggestion_menu_binding.as_ref() == Some(&binding)
            && self.terminal_command_suggestions.menu_is_open()
            && let Some(menu) = self.suggestion_menu.as_ref()
        {
            if terminal_event.is_some_and(|key| {
                matches!(key.key, TerminalKey::Enter | TerminalKey::Escape)
                    && key.phase == KeyPhase::Repeat
                    && key.modifiers == InputModifiers::default()
            }) {
                cx.stop_propagation();
                return;
            }
            let menu_key = terminal_event.as_ref().and_then(|event| match event.key {
                _ if event.modifiers != InputModifiers::default() => None,
                TerminalKey::Up => Some(command_suggestions::TerminalCommandSuggestionMenuKey::Up),
                TerminalKey::Down => {
                    Some(command_suggestions::TerminalCommandSuggestionMenuKey::Down)
                }
                TerminalKey::Enter if event.phase == KeyPhase::Press => {
                    Some(command_suggestions::TerminalCommandSuggestionMenuKey::Enter)
                }
                TerminalKey::Escape if event.phase == KeyPhase::Press => {
                    Some(command_suggestions::TerminalCommandSuggestionMenuKey::Escape)
                }
                _ => None,
            });
            if let Some(menu_key) = menu_key {
                self.keybinding_runtime.clear_pending();
                if menu.update(cx, |menu, cx| menu.handle_key(menu_key, cx)) {
                    if matches!(
                        menu_key,
                        command_suggestions::TerminalCommandSuggestionMenuKey::Enter
                            | command_suggestions::TerminalCommandSuggestionMenuKey::Escape
                    ) {
                        self.consume_local_terminal_key(event, window, cx);
                    } else {
                        cx.stop_propagation();
                    }
                    return;
                }
                self.terminal_command_suggestions.dismiss_menu();
                self.suggestion_menu_binding = None;
                self.sync_suggestion_menu(cx);
                cx.notify();
            }
        }

        if suggestion_anchor_available
            && !event.prefer_character_input
            && let Some(terminal_event) = terminal_event.as_ref()
        {
            if !native_prediction_visible
                && terminal_event.key == TerminalKey::Escape
                && terminal_event.phase == KeyPhase::Press
                && terminal_event.modifiers == InputModifiers::default()
                && self
                    .terminal_command_suggestions
                    .inline_ghost()
                    .is_some_and(|ghost| ghost.binding == binding)
                && self.terminal_command_suggestions.dismiss_inline()
            {
                self.consume_local_terminal_key(event, window, cx);
                cx.notify();
                return;
            }
            if self
                .terminal_command_suggestions
                .is_menu_shortcut(*terminal_event)
                && self.request_suggestion_snapshot(
                    binding.clone(),
                    SuggestionQueryMode::Menu,
                    true,
                    cx,
                )
            {
                self.consume_local_terminal_key(event, window, cx);
                return;
            }
            let suggestion_context = self.suggestion_context_for_binding(&binding);
            if !native_prediction_visible
                && let Some(context) = suggestion_context.as_ref()
                && let Some(acceptance) = self
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
                    self.consume_local_terminal_key(event, window, cx);
                    return;
                }
            }
        }

        if plain_escape_press
            && (self.close_terminal_search_for_binding(&binding, window, cx)
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
            self.consume_local_terminal_key(event, window, cx);
            return;
        }

        // Windows/Linux 的 Ctrl+Insert 默认复制可能被用户改写为终端动作：先让现有 keymap
        // 解析并缓存结果，命中时跳过本地复制，交由下面的统一动作分发处理。
        let ctrl_insert_dispatch = terminal_event.and_then(|terminal_event| {
            resolve_ctrl_insert_override(
                &mut self.keybinding_runtime,
                terminal_event.key,
                event,
                ime_composing,
            )
        });
        let ctrl_insert_override = ctrl_insert_override_wins(ctrl_insert_dispatch.as_ref());

        if !ctrl_insert_override
            && let Some(action) = terminal_event.and_then(|terminal_event| {
                terminal_clipboard_shortcut(terminal_event.key, event, has_selection, ime_composing)
            })
        {
            if event.is_held && action != TerminalClipboardAction::Interrupt {
                // 首次 press 已在下面登记物理主键；修饰键先松开后的重复 event 继续按复制手势吞掉。
                self.keybinding_runtime.clear_pending();
                cx.stop_propagation();
                return;
            }
            match action {
                TerminalClipboardAction::CopySelection => {
                    // 显式复制手势始终交给所属选择工具；没有选择时 selected_text 返回 None，
                    // 既不改写剪贴板也不向 PTY 写入，这里只负责消费按键并给出反馈。
                    if !dispatch_selection_action(
                        self,
                        &binding,
                        TerminalToolsEvent::CopySelection {
                            session_id: binding.session_id.clone(),
                        },
                        true,
                        window,
                        cx,
                    ) {
                        self.status = Some(crate::locale::text(
                            "当前没有可复制的终端选择。",
                            "There is no terminal selection to copy.",
                        ));
                        cx.notify();
                    }
                    // 记录已消费的物理主键，让 keyup 和修饰键先松开的重复键都不进入 PTY/Kitty。
                    self.keybinding_runtime
                        .remember_terminal_shortcut_key(&event.keystroke);
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

        let dispatch = ctrl_insert_dispatch.unwrap_or_else(|| {
            resolve_terminal_shortcut(&mut self.keybinding_runtime, event, ime_composing)
        });
        match dispatch {
            keybindings::KeybindingDispatch::Unhandled => {}
            keybindings::KeybindingDispatch::Plugin(id) => {
                self.keybinding_runtime
                    .remember_terminal_shortcut_key(&event.keystroke);
                self.run_external_plugin_command(id, cx);
                cx.stop_propagation();
                return;
            }
            keybindings::KeybindingDispatch::Pending => {
                self.keybinding_runtime
                    .remember_terminal_shortcut_key(&event.keystroke);
                cx.stop_propagation();
                return;
            }
            keybindings::KeybindingDispatch::Action(action) => {
                self.keybinding_runtime
                    .remember_terminal_shortcut_key(&event.keystroke);
                if !terminal_action_can_target_workspace(action, &binding.owner) {
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if action == KeybindingAction::TerminalPaste {
                    if !paste_bound_terminal(self, &binding, input_handler.clone(), cx) {
                        self.status = Some(crate::locale::text(
                            "当前终端不可用，剪贴板内容未能粘贴。",
                            "The focused terminal is unavailable; clipboard text was not pasted.",
                        ));
                        cx.notify();
                    }
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
                self.keybinding_runtime
                    .release_terminal_shortcut_key(&event.keystroke);
                if ctrl_insert_override {
                    // 已命中的 Ctrl+Insert 不能在动作暂不可达时回落成 Insert 协议编码漏进 PTY。
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
            }
        }

        if let Some(scroll) = terminal_event.and_then(|terminal_event| {
            viewport_scroll_action(terminal_event.key, modifiers, modes, page_rows)
        }) {
            // 已经识别的本地滚动即使排队失败也不能回落到 PTY：Shift+PageUp 等会变成 ANSI 序列。
            self.keybinding_runtime.clear_pending();
            let _ = dispatch_viewport_scroll(self, &binding, scroll, cx);
            // 登记滚动归属，使重复事件和 keyup 都遵守首次按下的路由。
            self.keybinding_runtime
                .remember_terminal_scroll_key(&event.keystroke);
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
        // 修饰键先松开时，仍由首次消费方接管，避免长按变成字符提示或默认滚动。
        if consumed_terminal_shortcut_repeat(&self.keybinding_runtime, event) {
            return true;
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

        // 历史 pane 与活动终端一致：已配置的 Ctrl+Insert 优先，未命中才复制只读选择。
        let ctrl_insert_dispatch = terminal_event.and_then(|terminal_event| {
            resolve_ctrl_insert_override(
                &mut self.keybinding_runtime,
                terminal_event.key,
                event,
                false,
            )
        });
        let ctrl_insert_override = ctrl_insert_override_wins(ctrl_insert_dispatch.as_ref());

        if !ctrl_insert_override
            && let Some(action) = terminal_event.and_then(|key| {
                // 历史 pane 没有输入法组合状态，但仍要遵守平台声明的字符输入优先级。
                terminal_clipboard_shortcut(key.key, event, has_selection, false)
            })
        {
            if event.is_held && action != TerminalClipboardAction::Interrupt {
                self.keybinding_runtime.clear_pending();
                return true;
            }
            match action {
                TerminalClipboardAction::CopySelection => {
                    // 与 live pane 相同：显式复制手势始终消费；没有选择时选择工具返回 None，
                    // 不会写剪贴板，也不会因为只读而回落到任何输入通道。
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
            }
            self.keybinding_runtime.clear_pending();
            return true;
        }

        // 已退出或断开的终端没有可写 PTY；字符与回车不再静默丢弃，直接把唯一恢复入口告诉用户。
        if matches!(
            terminal_event.map(|key| key.key),
            Some(TerminalKey::Character(_) | TerminalKey::Enter)
        ) && !modifiers.control
            && !modifiers.alt
            && !modifiers.super_key
        {
            self.status = Some(crate::locale::text(
                "终端未连接，历史内容只读；点击标题栏的重新连接可恢复输入。",
                "The terminal is not connected and its history is read-only; click Reconnect in the title bar to resume typing.",
            ));
            cx.notify();
            self.keybinding_runtime.clear_pending();
            return true;
        }

        // 与活动终端一样让显式绑定先于滚动。长按继续由首次消费方处理，避免改绑后回落到翻页。
        let dispatch = ctrl_insert_dispatch.unwrap_or_else(|| {
            resolve_terminal_shortcut(&mut self.keybinding_runtime, event, false)
        });
        match dispatch {
            keybindings::KeybindingDispatch::Unhandled => {}
            keybindings::KeybindingDispatch::Pending => {
                self.keybinding_runtime
                    .remember_terminal_shortcut_key(&event.keystroke);
                return true;
            }
            keybindings::KeybindingDispatch::Plugin(id) => {
                self.keybinding_runtime
                    .remember_terminal_shortcut_key(&event.keystroke);
                self.run_external_plugin_command(id, cx);
                return true;
            }
            keybindings::KeybindingDispatch::Action(action) => {
                let consumed = match action {
                    action if !terminal_action_can_target_workspace(action, &binding.owner) => true,
                    KeybindingAction::TerminalSearch => {
                        self.open_terminal_search_for_binding(&binding, window, cx);
                        true
                    }
                    crate::keybindings::KeybindingAction::TerminalPaste => {
                        self.status = Some(crate::locale::text(
                            "终端已退出，历史内容只读，无法粘贴。",
                            "The terminal has exited; its history is read-only and cannot receive paste input.",
                        ));
                        cx.notify();
                        true
                    }
                    _ => self.dispatch_keybinding_action(action, window, cx),
                };
                if consumed {
                    self.keybinding_runtime
                        .remember_terminal_shortcut_key(&event.keystroke);
                    return true;
                }
                if ctrl_insert_override {
                    // 配置命中的 Ctrl+Insert 在只读历史里即使动作暂时不可达也不能继续下传。
                    return true;
                }
            }
        }

        if let Some(key) = terminal_event
            && let Some(scroll) =
                viewport_scroll_action_for_keys(key.key, modifiers, snapshot.rows.len().max(1))
        {
            self.keybinding_runtime.clear_pending();
            let _ = dispatch_viewport_scroll(self, &binding, scroll, cx);
            return true;
        }
        false
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
