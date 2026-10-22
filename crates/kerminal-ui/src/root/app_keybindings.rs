//! Root 工作区祖先快捷键适配；全局快捷键统一复用现有解析运行时。
//!
//! @author kongweiguang

use gpui_kit::{Context, KeyDownEvent, Window};

use super::{
    RootView,
    keybindings::{KeybindingDispatch, KeybindingFocusScope, RootKeybindingRuntime},
};

/// 在工作区祖先处理 App/Workspace 快捷键，并仅将 Editor 中的退出与设置动作送入既有入口。
///
/// GPUI 会跳过已经被子控件停止传播的事件，因此这里不重复处理终端 Canvas 的快捷键；Editor 只开放
/// 独立编译白名单中的动作，其他编辑键、IME 和 Modal 输入仍由焦点控件处理。
///
/// @author kongweiguang
pub(super) fn handle_app_key_down(
    root: &mut RootView,
    event: &KeyDownEvent,
    focus_scope: KeybindingFocusScope,
    ime_composing: bool,
    text_input_focused: bool,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    match resolve_app_key(
        &mut root.keybinding_runtime,
        event,
        focus_scope,
        ime_composing,
        text_input_focused,
    ) {
        KeybindingDispatch::Unhandled => false,
        KeybindingDispatch::Pending => {
            cx.stop_propagation();
            true
        }
        KeybindingDispatch::Action(action) => {
            if root.dispatch_keybinding_action(action, window, cx) {
                cx.stop_propagation();
                true
            } else {
                false
            }
        }
    }
}

/// App/Workspace 使用完整 keymap；Editor 仅借用退出和打开设置动作的独立受限 keymap。
///
/// @author kongweiguang
fn resolve_app_key(
    runtime: &mut RootKeybindingRuntime,
    event: &KeyDownEvent,
    focus_scope: KeybindingFocusScope,
    ime_composing: bool,
    text_input_focused: bool,
) -> KeybindingDispatch {
    if focus_scope == KeybindingFocusScope::Editor {
        return runtime.dispatch_editor_shortcut(
            &event.keystroke,
            ime_composing,
            event.prefer_character_input,
            event.is_held,
        );
    }

    if !matches!(
        focus_scope,
        KeybindingFocusScope::App | KeybindingFocusScope::Workspace
    ) || event.prefer_character_input
    {
        runtime.clear_pending();
        return KeybindingDispatch::Unhandled;
    }

    runtime.dispatch(
        &event.keystroke,
        focus_scope,
        ime_composing,
        text_input_focused,
        event.is_held,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/root_app_keybindings.rs"]
mod tests;
