//! Root 快捷键作用域投影的独立测试。
//!
//! @author kongweiguang

use super::{KeybindingDispatch, KeybindingFocusScope, RootKeybindingRuntime, context_stack};
use crate::keybindings::KeybindingContext;
use gpui_kit::Keystroke;
use kerminal_application::preferences::{KeybindingPreference, KeybindingScope};

/// 用固定配置构造编译器输入，使测试不读取用户目录或依赖平台设置。
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

/// 验证应用焦点只暴露全局上下文，不意外开放工作区或终端快捷键。
#[test]
fn app_scope_contains_only_app_context() {
    let stack = context_stack(KeybindingFocusScope::App).expect("app focus has a context");

    assert_eq!(stack.len(), 1);
    assert!(stack[0].contains(KeybindingContext::App.as_gpui_identifier()));
    assert!(!stack[0].contains(KeybindingContext::Workspace.as_gpui_identifier()));
    assert!(!stack[0].contains(KeybindingContext::Terminal.as_gpui_identifier()));
    assert!(!stack[0].contains(KeybindingContext::TextInput.as_gpui_identifier()));
}

/// 验证工作区和终端上下文按父子顺序组成，匹配已编译绑定的层级 predicate。
#[test]
fn workspace_and_terminal_scopes_preserve_context_hierarchy() {
    let workspace =
        context_stack(KeybindingFocusScope::Workspace).expect("workspace focus has a context");
    assert_eq!(workspace.len(), 2);
    assert!(workspace[0].contains(KeybindingContext::App.as_gpui_identifier()));
    assert!(workspace[1].contains(KeybindingContext::Workspace.as_gpui_identifier()));

    let terminal =
        context_stack(KeybindingFocusScope::Terminal).expect("terminal focus has a context");
    assert_eq!(terminal.len(), 3);
    assert!(terminal[0].contains(KeybindingContext::App.as_gpui_identifier()));
    assert!(terminal[1].contains(KeybindingContext::Workspace.as_gpui_identifier()));
    assert!(terminal[2].contains(KeybindingContext::Terminal.as_gpui_identifier()));
}

/// 验证编辑器上下文带有输入控件标记，应用快捷键 resolver 会让位给文本和 IME 输入。
#[test]
fn editor_scope_is_marked_as_text_input() {
    let stack = context_stack(KeybindingFocusScope::Editor).expect("editor focus has a context");

    assert_eq!(stack.len(), 3);
    assert!(stack[2].contains(KeybindingContext::TextInput.as_gpui_identifier()));
    assert!(!stack[2].contains(KeybindingContext::Terminal.as_gpui_identifier()));
}

/// 验证模态弹层不进入应用 keymap 分发链，避免全局快捷键抢占确认和取消操作。
#[test]
fn modal_scope_disables_application_keymap_dispatch() {
    assert!(context_stack(KeybindingFocusScope::Modal).is_none());
}

/// 按 Root 运行时验证匹配动作返回给调用方，而未匹配键保持原样透传。
#[test]
fn runtime_dispatches_only_registered_actions() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "settings.open",
        "Ctrl+Alt+S",
        KeybindingScope::Global,
    )]);

    let action = runtime.dispatch(
        &Keystroke::parse("ctrl-alt-s").expect("valid shortcut"),
        KeybindingFocusScope::Terminal,
        false,
        false,
        false,
    );
    assert_eq!(
        action,
        KeybindingDispatch::Action(crate::keybindings::KeybindingAction::SettingsOpen)
    );

    let plain = runtime.dispatch(
        &Keystroke::parse("x").expect("valid text key"),
        KeybindingFocusScope::Terminal,
        false,
        false,
        false,
    );
    assert_eq!(plain, KeybindingDispatch::Unhandled);
}

/// 确认 Shift+字符仍作为文本透传，而无需修饰键的导航键仍可绑定。
#[test]
fn runtime_does_not_consume_shifted_text_strokes() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[
        preference("terminal.newTab", "Shift+X", KeybindingScope::Workspace),
        preference("terminal.nextTab", "Tab", KeybindingScope::Workspace),
    ]);

    assert_eq!(
        runtime.dispatch(
            &Keystroke::parse("shift-x").expect("valid shifted text key"),
            KeybindingFocusScope::Workspace,
            false,
            false,
            false,
        ),
        KeybindingDispatch::Unhandled
    );
    assert_eq!(
        runtime.dispatch(
            &Keystroke::parse("tab").expect("valid navigation key"),
            KeybindingFocusScope::Workspace,
            false,
            false,
            false,
        ),
        KeybindingDispatch::Action(crate::keybindings::KeybindingAction::TerminalNextTab)
    );
}

/// 原生导航键以无字符 keystroke 到达；只有真实 IME 预编辑状态才能阻止其快捷键。
#[test]
fn runtime_dispatches_native_tab_and_enter_but_yields_during_composition() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[
        preference("terminal.nextTab", "Tab", KeybindingScope::Workspace),
        preference("workspace.quickOpen", "Enter", KeybindingScope::Workspace),
    ]);
    let tab = Keystroke::parse("tab").expect("valid native navigation key");
    let enter = Keystroke::parse("enter").expect("valid native confirmation key");

    assert!(tab.key_char.is_none());
    assert!(enter.key_char.is_none());
    assert!(!tab.is_ime_in_progress());
    assert!(!enter.is_ime_in_progress());
    assert_eq!(
        runtime.dispatch(&tab, KeybindingFocusScope::Workspace, false, false, false),
        KeybindingDispatch::Action(crate::keybindings::KeybindingAction::TerminalNextTab)
    );
    assert_eq!(
        runtime.dispatch(&enter, KeybindingFocusScope::Workspace, false, false, false),
        KeybindingDispatch::Action(crate::keybindings::KeybindingAction::WorkspaceQuickOpen)
    );

    for keystroke in [&tab, &enter] {
        assert_eq!(
            runtime.dispatch(
                keystroke,
                KeybindingFocusScope::Workspace,
                true,
                false,
                false
            ),
            KeybindingDispatch::Unhandled
        );
    }
}

/// 多 stroke 绑定只暂存有效前缀，完成 chord 后返回动作，焦点变化则清除前缀。
#[test]
fn runtime_tracks_and_clears_chord_prefixes() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "terminal.newTab",
        "Ctrl+K Ctrl+T",
        KeybindingScope::Workspace,
    )]);

    let first = runtime.dispatch(
        &Keystroke::parse("ctrl-k").expect("valid first stroke"),
        KeybindingFocusScope::Workspace,
        false,
        false,
        false,
    );
    assert_eq!(first, KeybindingDispatch::Pending);

    let second = runtime.dispatch(
        &Keystroke::parse("ctrl-t").expect("valid second stroke"),
        KeybindingFocusScope::Workspace,
        false,
        false,
        false,
    );
    assert_eq!(
        second,
        KeybindingDispatch::Action(crate::keybindings::KeybindingAction::TerminalNewTab)
    );

    let pending = runtime.dispatch(
        &Keystroke::parse("ctrl-k").expect("valid first stroke"),
        KeybindingFocusScope::Workspace,
        false,
        false,
        false,
    );
    assert_eq!(pending, KeybindingDispatch::Pending);
    let text_focus = runtime.dispatch(
        &Keystroke::parse("ctrl-t").expect("valid second stroke"),
        KeybindingFocusScope::Editor,
        false,
        true,
        false,
    );
    assert_eq!(text_focus, KeybindingDispatch::Unhandled);
}

/// chord 前缀不匹配时，当前按键仍可独立触发其它动作，避免吞掉有效快捷键。
#[test]
fn runtime_retries_mismatched_chord_stroke_as_a_new_shortcut() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[
        preference(
            "terminal.newTab",
            "Ctrl+K Ctrl+T",
            KeybindingScope::Workspace,
        ),
        preference("settings.open", "Ctrl+Alt+S", KeybindingScope::Global),
    ]);

    assert_eq!(
        runtime.dispatch(
            &Keystroke::parse("ctrl-k").expect("valid chord prefix"),
            KeybindingFocusScope::Workspace,
            false,
            false,
            false,
        ),
        KeybindingDispatch::Pending
    );
    assert_eq!(
        runtime.dispatch(
            &Keystroke::parse("ctrl-alt-s").expect("valid independent shortcut"),
            KeybindingFocusScope::Workspace,
            false,
            false,
            false,
        ),
        KeybindingDispatch::Action(crate::keybindings::KeybindingAction::SettingsOpen)
    );
}

/// IME 组合、重复按键和文本编辑焦点都不执行快捷键，保证系统输入与编辑语义优先。
#[test]
fn runtime_yields_to_ime_text_fields_and_repeats() {
    let mut runtime = RootKeybindingRuntime::default();
    runtime.update(&[preference(
        "settings.open",
        "Ctrl+Alt+S",
        KeybindingScope::Global,
    )]);
    let key = Keystroke::parse("ctrl-alt-s").expect("valid shortcut");

    for (scope, composing, text_focused, repeated) in [
        (KeybindingFocusScope::Terminal, true, false, false),
        (KeybindingFocusScope::Editor, false, true, false),
        (KeybindingFocusScope::Terminal, false, false, true),
    ] {
        assert_eq!(
            runtime.dispatch(&key, scope, composing, text_focused, repeated),
            KeybindingDispatch::Unhandled
        );
    }
}
