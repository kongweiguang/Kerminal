//! Root 工作区祖先快捷键适配的独立测试。
//!
//! @author kongweiguang

use super::{KeybindingDispatch, KeybindingFocusScope, RootKeybindingRuntime, resolve_app_key};
use crate::keybindings::KeybindingAction;
use gpui_kit::{KeyDownEvent, Keystroke};
use kerminal_application::preferences::{KeybindingPreference, KeybindingScope};

/// 构造显式绑定 DTO，避免测试读取用户配置或依赖平台默认值。
///
/// @author kongweiguang
fn preference(action: &str, binding: &str, scope: KeybindingScope) -> KeybindingPreference {
    KeybindingPreference {
        action: action.to_owned(),
        label: action.to_owned(),
        description: String::new(),
        binding: binding.to_owned(),
        windows_binding: binding.to_owned(),
        mac_binding: binding.to_owned(),
        linux_binding: None,
        windows_alternate_binding: None,
        scope,
        editable: true,
    }
}

/// 构造 GPUI 原生按键事件，逐项控制重复键和 AltGr 字符优先语义。
fn key_down(binding: &str, is_held: bool, prefer_character_input: bool) -> KeyDownEvent {
    KeyDownEvent {
        keystroke: Keystroke::parse(binding).expect("valid key stroke"),
        is_held,
        prefer_character_input,
    }
}

/// Root 只在应用/工作区作用域分发平台默认退出键，并让裸 Ctrl+Q 继续透传。
///
/// @author kongweiguang
#[test]
fn application_quit_uses_platform_shortcut_without_claiming_ctrl_q() {
    let mut runtime = RootKeybindingRuntime::default();
    let binding = if cfg!(target_os = "macos") {
        "Cmd+Q"
    } else {
        "Ctrl+Shift+Q"
    };
    runtime.update(&[preference(
        "application.quit",
        binding,
        KeybindingScope::Global,
    )]);
    let quit_key = if cfg!(target_os = "macos") {
        "cmd-q"
    } else {
        "ctrl-shift-q"
    };

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down(quit_key, false, false),
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Action(KeybindingAction::ApplicationQuit)
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-q", false, false),
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Unhandled
    );
}

/// 编辑器文本焦点只开放退出和设置入口；其余全局动作及 IME/字符输入仍保留给原控件。
///
/// @author kongweiguang
#[test]
fn editor_focus_allows_only_quit_and_preserves_input_guards() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[
        preference("application.quit", "Ctrl+Shift+Q", KeybindingScope::Global),
        preference("settings.open", "Ctrl+Alt+S", KeybindingScope::Global),
        preference("tool.system", "Ctrl+Alt+Y", KeybindingScope::Global),
    ]);

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-shift-q", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Action(KeybindingAction::ApplicationQuit)
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-alt-s", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Action(KeybindingAction::SettingsOpen),
        "settings remains reachable while an editor input owns focus"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-alt-y", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "editor focus must not expand the whitelist to other global shortcuts"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-shift-q", false, false),
            KeybindingFocusScope::Editor,
            true,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "active IME composition blocks the exit shortcut"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-shift-q", false, true),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "character-preferred events remain with the text input"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-shift-q", true, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "held key repeats cannot request application exit"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-alt-s", false, false),
            KeybindingFocusScope::Editor,
            true,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "active IME composition blocks the settings shortcut too"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-alt-s", false, true),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "character-preferred events remain with the text input"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-alt-s", true, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "held repeats do not reopen settings"
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-shift-q", false, false),
            KeybindingFocusScope::Modal,
            false,
            false,
        ),
        KeybindingDispatch::Unhandled
    );
}

/// 编辑器的受限 keymap 仍使用统一 compiler，因此多 stroke 与原配置作用范围保持有效。
///
/// @author kongweiguang
#[test]
fn editor_shortcut_chord_respects_compiled_action_and_scope() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[
        preference("settings.open", "Ctrl+K Ctrl+S", KeybindingScope::Global),
        preference("tool.system", "Ctrl+K Ctrl+Y", KeybindingScope::Global),
    ]);

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-k", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Pending
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-s", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Action(KeybindingAction::SettingsOpen)
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-k", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Pending
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-y", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "the editor runtime compiles only its explicit action whitelist"
    );

    let mut scoped_runtime = RootKeybindingRuntime::default();
    scoped_runtime.update(&[preference(
        "settings.open",
        "Ctrl+Alt+S",
        KeybindingScope::Workspace,
    )]);
    assert_eq!(
        resolve_app_key(
            &mut scoped_runtime,
            &key_down("ctrl-alt-s", false, false),
            KeybindingFocusScope::Editor,
            false,
            true,
        ),
        KeybindingDispatch::Unhandled,
        "the restricted runtime preserves the configured context predicate"
    );
}

/// 全局快捷键在 App 与 Workspace context 中均可解析，不复制动作映射规则。
#[test]
fn app_scope_global_binding_resolves_in_app_and_workspace() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "settings.open",
        "Ctrl+Alt+S",
        KeybindingScope::Global,
    )]);
    let event = key_down("ctrl-alt-s", false, false);

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &event,
            KeybindingFocusScope::App,
            false,
            false,
        ),
        KeybindingDispatch::Action(KeybindingAction::SettingsOpen)
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &event,
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Action(KeybindingAction::SettingsOpen)
    );
}

/// Workspace 绑定只在工作区 context 命中，App 导航焦点不会误触发终端动作。
#[test]
fn workspace_binding_is_scoped_to_workspace_focus() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "terminal.newTab",
        "Ctrl+Shift+N",
        KeybindingScope::Workspace,
    )]);
    let event = key_down("ctrl-shift-n", false, false);

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &event,
            KeybindingFocusScope::App,
            false,
            false,
        ),
        KeybindingDispatch::Unhandled
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &event,
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Action(KeybindingAction::TerminalNewTab)
    );
}

/// 多 stroke 前缀只在同一 Workspace 焦点内完成，焦点越界会清掉待续 chord。
#[test]
fn workspace_chord_completes_and_focus_change_clears_prefix() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "terminal.newTab",
        "Ctrl+K Ctrl+T",
        KeybindingScope::Workspace,
    )]);
    let prefix = key_down("ctrl-k", false, false);
    let suffix = key_down("ctrl-t", false, false);

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &prefix,
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Pending
    );
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &suffix,
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Action(KeybindingAction::TerminalNewTab)
    );

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &prefix,
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Pending
    );
    for scope in [
        KeybindingFocusScope::Terminal,
        KeybindingFocusScope::Editor,
        KeybindingFocusScope::Modal,
    ] {
        assert_eq!(
            resolve_app_key(&mut runtime, &suffix, scope, false, false),
            KeybindingDispatch::Unhandled
        );
    }
    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &suffix,
            KeybindingFocusScope::Workspace,
            false,
            false,
        ),
        KeybindingDispatch::Unhandled
    );
}

/// IME、文本焦点、重复键和 AltGr 字符优先事件全部保持给系统输入路径。
#[test]
fn text_input_ime_repeat_and_character_preference_are_not_intercepted() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "settings.open",
        "Ctrl+Alt+S",
        KeybindingScope::Global,
    )]);

    for (event, ime_composing, text_input_focused) in [
        (key_down("ctrl-alt-s", false, false), true, false),
        (key_down("ctrl-alt-s", false, false), false, true),
        (key_down("ctrl-alt-s", true, false), false, false),
        (key_down("ctrl-alt-s", false, true), false, false),
    ] {
        assert_eq!(
            resolve_app_key(
                &mut runtime,
                &event,
                KeybindingFocusScope::Workspace,
                ime_composing,
                text_input_focused,
            ),
            KeybindingDispatch::Unhandled
        );
    }
}

/// Terminal 作用域保持 Canvas 的单一分发权，外层 adapter 不再解析或触发同一动作。
#[test]
fn terminal_scope_is_left_to_terminal_canvas_dispatcher() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "terminal.newTab",
        "Ctrl+Shift+N",
        KeybindingScope::Terminal,
    )]);

    assert_eq!(
        resolve_app_key(
            &mut runtime,
            &key_down("ctrl-shift-n", false, false),
            KeybindingFocusScope::Terminal,
            false,
            false,
        ),
        KeybindingDispatch::Unhandled
    );
}
