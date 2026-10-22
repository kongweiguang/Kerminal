//! 原生快捷键编译与派发契约测试。
//!
//! @author kongweiguang

#[path = "../../src/keybindings.rs"]
mod keybinding_compiler;

use self::keybinding_compiler::{
    CompiledKeybindings, KeybindingAction, KeybindingContext, KeybindingDiagnosticKind,
    KeybindingPlatform,
};
use gpui_kit::{KeyContext, Keystroke};
use kerminal_application::preferences::{KeybindingPreference, KeybindingScope, SettingsSnapshot};

/// 构造包含新增可选平台字段的测试配置，避免测试依赖用户配置文件。
///
/// @author kongweiguang
fn preference(
    action: &str,
    binding: &str,
    windows_binding: &str,
    mac_binding: &str,
    scope: KeybindingScope,
) -> KeybindingPreference {
    KeybindingPreference {
        action: action.to_owned(),
        label: action.to_owned(),
        description: String::new(),
        binding: binding.to_owned(),
        windows_binding: windows_binding.to_owned(),
        mac_binding: mac_binding.to_owned(),
        linux_binding: None,
        windows_alternate_binding: None,
        scope,
        editable: true,
    }
}

/// 创建带有应用、工作区和终端层级的 GPUI context stack。
fn terminal_context_stack() -> Vec<KeyContext> {
    let mut app = KeyContext::new_with_defaults();
    app.add(KeybindingContext::App.as_gpui_identifier());
    let mut workspace = KeyContext::default();
    workspace.add(KeybindingContext::Workspace.as_gpui_identifier());
    let mut terminal = KeyContext::default();
    terminal.add(KeybindingContext::Terminal.as_gpui_identifier());
    vec![app, workspace, terminal]
}

/// 旧配置动作和新增导航/搜索动作都必须进入 typed 动作白名单。
///
/// @author kongweiguang
#[test]
fn maps_all_frozen_default_action_ids() {
    let ids = [
        "application.quit",
        "settings.open",
        "settings.keybindings",
        "terminal.focus",
        "terminal.search",
        "terminal.paste",
        "terminal.newTab",
        "terminal.closeTab",
        "terminal.closePane",
        "terminal.splitHorizontal",
        "terminal.splitVertical",
        "terminal.previousTab",
        "terminal.nextTab",
        "tool.agentLauncher",
        "tool.system",
        "tool.sftp",
        "tool.ports",
        "tool.snippets",
        "tool.logs",
        "workspace.quickOpen",
        "workspace.commandPalette",
    ];

    for id in ids {
        let action = KeybindingAction::from_id(id).expect("frozen action maps");
        assert_eq!(action.id(), id);
    }
    assert_eq!(KeybindingAction::from_id("future.action"), None);
}

/// 退出快捷键使用平台约定的修饰键，Windows 的裸 Ctrl+Q 继续留给终端 XON。
///
/// @author kongweiguang
#[test]
fn application_quit_binding_is_platform_specific_and_preserves_ctrl_q() {
    let preferences = SettingsSnapshot::default().keybindings;
    let contexts = terminal_context_stack();
    let windows = CompiledKeybindings::compile(&preferences, KeybindingPlatform::Windows);
    let mac = CompiledKeybindings::compile(&preferences, KeybindingPlatform::Mac);

    for compiled in [&windows, &mac] {
        assert!(!compiled.diagnostics().iter().any(|diagnostic| {
            diagnostic.kind == KeybindingDiagnosticKind::Conflict
                && (diagnostic.action == "application.quit"
                    || diagnostic.conflicts_with.as_deref() == Some("application.quit"))
        }));
    }

    assert_eq!(
        windows
            .resolve(
                &[Keystroke::parse("ctrl-shift-q").expect("parse Windows quit shortcut")],
                &contexts,
                false,
                false,
                false,
            )
            .action,
        Some(KeybindingAction::ApplicationQuit)
    );
    assert_eq!(
        mac.resolve(
            &[Keystroke::parse("cmd-q").expect("parse macOS quit shortcut")],
            &contexts,
            false,
            false,
            false,
        )
        .action,
        Some(KeybindingAction::ApplicationQuit)
    );
    assert_eq!(
        windows
            .resolve(
                &[Keystroke::parse("ctrl-q").expect("parse terminal XON")],
                &contexts,
                false,
                false,
                false,
            )
            .action,
        None
    );
}

/// 终端搜索仅在 Terminal scope 命中；无该上下文时 Ctrl+F 保持终端自身输入语义。
/// 两个平台借用同一份配置进行编译，保留平台差异而无需复制配置。
///
/// @author kongweiguang
#[test]
fn terminal_search_binding_is_scoped_and_platform_configurable() {
    let search = preference(
        "terminal.search",
        "",
        "Ctrl+F",
        "Cmd+F",
        KeybindingScope::Terminal,
    );
    let windows =
        CompiledKeybindings::compile(std::slice::from_ref(&search), KeybindingPlatform::Windows);
    let mac = CompiledKeybindings::compile(&[search], KeybindingPlatform::Mac);
    let keystroke = Keystroke::parse("ctrl-f").expect("parse Ctrl+F");
    let workspace_contexts = terminal_context_stack()[..2].to_vec();

    assert_eq!(
        windows
            .resolve(
                std::slice::from_ref(&keystroke),
                &terminal_context_stack(),
                false,
                false,
                false,
            )
            .action,
        Some(KeybindingAction::TerminalSearch)
    );
    assert_eq!(
        windows
            .resolve(
                std::slice::from_ref(&keystroke),
                &workspace_contexts,
                false,
                false,
                false,
            )
            .action,
        None
    );
    assert_eq!(
        mac.resolve(
            &[Keystroke::parse("cmd-f").expect("parse Cmd+F")],
            &terminal_context_stack(),
            false,
            false,
            false,
        )
        .action,
        Some(KeybindingAction::TerminalSearch)
    );
}

/// 命令面板复用唯一内置动作目录，避免 action 清单与 Root 分发白名单逐渐分叉。
#[test]
fn builtin_action_directory_has_unique_roundtrippable_ids() {
    let ids = KeybindingAction::ALL
        .into_iter()
        .map(|action| {
            assert_eq!(KeybindingAction::from_id(action.id()), Some(action));
            action.id()
        })
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(ids.len(), KeybindingAction::ALL.len());
    assert!(ids.contains("workspace.quickOpen"));
    assert!(ids.contains("workspace.commandPalette"));
}

/// macOS/Windows override、旧修饰键别名与通用绑定回退保持原配置语义。
/// 编译器借用覆盖配置，验证不同平台时不需要额外克隆输入。
///
/// @author kongweiguang
#[test]
fn selects_platform_override_and_falls_back_to_common_binding() {
    let platform_override = preference(
        "settings.open",
        "Ctrl+Alt+S",
        "Ctrl+Shift+X",
        "Cmd+Shift+Y",
        KeybindingScope::Global,
    );
    let contexts = terminal_context_stack();

    let windows = CompiledKeybindings::compile(
        std::slice::from_ref(&platform_override),
        KeybindingPlatform::Windows,
    );
    assert_eq!(
        windows
            .resolve(
                &[Keystroke::parse("ctrl-shift-x").expect("parse Windows key")],
                &contexts,
                false,
                false,
                false,
            )
            .action,
        Some(KeybindingAction::SettingsOpen)
    );

    let mac = CompiledKeybindings::compile(&[platform_override], KeybindingPlatform::Mac);
    assert_eq!(
        mac.resolve(
            &[Keystroke::parse("cmd-shift-y").expect("parse Mac key")],
            &contexts,
            false,
            false,
            false,
        )
        .action,
        Some(KeybindingAction::SettingsOpen)
    );

    let fallback = preference(
        "terminal.focus",
        "Ctrl+Option+Right",
        "",
        "",
        KeybindingScope::Global,
    );
    let fallback = CompiledKeybindings::compile(&[fallback], KeybindingPlatform::Mac);
    assert_eq!(
        fallback
            .resolve(
                &[Keystroke::parse("ctrl-alt-right").expect("parse aliased binding")],
                &contexts,
                false,
                false,
                false,
            )
            .action,
        Some(KeybindingAction::TerminalFocus)
    );
}

/// 通用和平台绑定都为空时必须关闭动作，而不是偷偷恢复默认快捷键。
#[test]
fn empty_effective_binding_disables_action() {
    let disabled = preference("terminal.newTab", "", "", "", KeybindingScope::Workspace);
    let compiled = CompiledKeybindings::compile(&[disabled], KeybindingPlatform::Windows);
    assert_eq!(
        compiled
            .resolve(
                &[Keystroke::parse("ctrl-shift-t").expect("parse key")],
                &terminal_context_stack(),
                false,
                false,
                false,
            )
            .action,
        None
    );
}

/// Windows 主/备用组合、Linux 专用绑定与 macOS 粘贴键由同一 typed action 覆盖。
/// 本机选择还需通过实际组合键解析，确保 Ctrl/Cmd 与 Windows 备用键按当前平台生效。
///
/// @author kongweiguang
#[test]
fn terminal_paste_uses_platform_defaults_and_windows_alternate() {
    let mut paste = preference(
        "terminal.paste",
        "",
        "Ctrl+V",
        "Cmd+V",
        KeybindingScope::Terminal,
    );
    paste.linux_binding = Some("Ctrl+Shift+V".to_owned());
    paste.windows_alternate_binding = Some("Ctrl+Shift+V".to_owned());
    let contexts = terminal_context_stack();

    let current =
        CompiledKeybindings::compile(std::slice::from_ref(&paste), KeybindingPlatform::current());
    for (shortcut, enabled) in [
        ("ctrl-v", cfg!(target_os = "windows")),
        ("ctrl-shift-v", !cfg!(target_os = "macos")),
        ("cmd-v", cfg!(target_os = "macos")),
    ] {
        assert_eq!(
            current
                .resolve(
                    &[Keystroke::parse(shortcut).expect("valid native paste shortcut")],
                    &contexts,
                    false,
                    false,
                    false,
                )
                .action,
            enabled.then_some(KeybindingAction::TerminalPaste)
        );
    }

    for (platform, shortcut) in [
        (KeybindingPlatform::Windows, "ctrl-v"),
        (KeybindingPlatform::Windows, "ctrl-shift-v"),
        (KeybindingPlatform::Linux, "ctrl-shift-v"),
        (KeybindingPlatform::Mac, "cmd-v"),
    ] {
        let compiled = CompiledKeybindings::compile(&[paste.clone()], platform);
        assert_eq!(
            compiled
                .resolve(
                    &[Keystroke::parse(shortcut).expect("valid paste shortcut")],
                    &contexts,
                    false,
                    false,
                    false,
                )
                .action,
            Some(KeybindingAction::TerminalPaste)
        );
    }

    let linux = CompiledKeybindings::compile(&[paste], KeybindingPlatform::Linux);
    assert_eq!(
        linux
            .resolve(
                &[Keystroke::parse("ctrl-v").expect("valid control-V key")],
                &contexts,
                false,
                false,
                false,
            )
            .action,
        None,
        "Linux Ctrl+V remains available to the terminal as a control key"
    );
}

/// Windows 主/备用绑定相同时仅注册一次同 action chord，不制造自身冲突诊断。
///
/// @author kongweiguang
#[test]
fn duplicate_windows_paste_chord_is_registered_once_without_self_conflict() {
    let mut paste = preference(
        "terminal.paste",
        "",
        "Ctrl+Shift+V",
        "Cmd+V",
        KeybindingScope::Terminal,
    );
    paste.windows_alternate_binding = Some("control+shift+v".to_owned());

    let compiled = CompiledKeybindings::compile(&[paste], KeybindingPlatform::Windows);
    assert!(!compiled.diagnostics().iter().any(|diagnostic| {
        diagnostic.kind == KeybindingDiagnosticKind::Conflict
            && diagnostic.action == "terminal.paste"
            && diagnostic.conflicts_with.as_deref() == Some("terminal.paste")
    }));
    assert_eq!(
        compiled
            .resolve(
                &[Keystroke::parse("ctrl-shift-v").expect("valid paste key")],
                &terminal_context_stack(),
                false,
                false,
                false,
            )
            .action,
        Some(KeybindingAction::TerminalPaste)
    );
}

/// Windows 主绑定和备用绑定分别保留冲突来源与绑定文本，方便设置页定位冲突。
///
/// @author kongweiguang
#[test]
fn windows_paste_conflicts_report_each_effective_binding() {
    let settings = preference("settings.open", "", "Ctrl+V", "", KeybindingScope::Global);
    let new_tab = preference(
        "terminal.newTab",
        "",
        "Ctrl+Shift+V",
        "",
        KeybindingScope::Workspace,
    );
    let mut paste = preference(
        "terminal.paste",
        "",
        "Ctrl+V",
        "Cmd+V",
        KeybindingScope::Terminal,
    );
    paste.windows_alternate_binding = Some("Ctrl+Shift+V".to_owned());

    let compiled =
        CompiledKeybindings::compile(&[settings, new_tab, paste], KeybindingPlatform::Windows);
    let paste_conflicts = compiled
        .diagnostics()
        .iter()
        .filter(|diagnostic| {
            diagnostic.kind == KeybindingDiagnosticKind::Conflict
                && diagnostic.action == "terminal.paste"
        })
        .map(|diagnostic| diagnostic.binding.as_str())
        .collect::<std::collections::BTreeSet<_>>();

    let expected = ["Ctrl+Shift+V", "Ctrl+V"]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(paste_conflicts, expected);
}

/// 显式空 Linux 字段关闭 paste，旧条目缺失该字段时仍回退到 Windows 覆盖值。
///
/// @author kongweiguang
#[test]
fn linux_binding_distinguishes_absent_fallback_from_explicit_empty() {
    let legacy = preference(
        "terminal.search",
        "Ctrl+F",
        "Ctrl+Shift+F",
        "Cmd+F",
        KeybindingScope::Terminal,
    );
    let legacy = CompiledKeybindings::compile(&[legacy], KeybindingPlatform::Linux);
    assert_eq!(
        legacy
            .resolve(
                &[Keystroke::parse("ctrl-shift-f").expect("valid legacy fallback")],
                &terminal_context_stack(),
                false,
                false,
                false,
            )
            .action,
        Some(KeybindingAction::TerminalSearch)
    );

    let mut disabled = preference(
        "terminal.paste",
        "Ctrl+Shift+V",
        "Ctrl+V",
        "Cmd+V",
        KeybindingScope::Terminal,
    );
    disabled.linux_binding = Some(String::new());
    let disabled = CompiledKeybindings::compile(&[disabled], KeybindingPlatform::Linux);
    for shortcut in ["ctrl-v", "ctrl-shift-v"] {
        assert_eq!(
            disabled
                .resolve(
                    &[Keystroke::parse(shortcut).expect("valid shortcut")],
                    &terminal_context_stack(),
                    false,
                    false,
                    false,
                )
                .action,
            None
        );
    }
}

/// GPUI context 深度让终端动作覆盖同键工作区和全局动作。
#[test]
fn deeper_scope_wins_and_overlapping_scopes_are_reported() {
    let preferences = [
        preference(
            "settings.open",
            "Ctrl+Alt+S",
            "",
            "",
            KeybindingScope::Global,
        ),
        preference(
            "terminal.newTab",
            "Ctrl+Alt+S",
            "",
            "",
            KeybindingScope::Workspace,
        ),
        preference(
            "terminal.closePane",
            "Ctrl+Alt+S",
            "",
            "",
            KeybindingScope::Terminal,
        ),
    ];
    let compiled = CompiledKeybindings::compile(&preferences, KeybindingPlatform::Windows);
    let conflicts = compiled
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.kind == KeybindingDiagnosticKind::Conflict)
        .count();
    assert_eq!(conflicts, 3);

    let resolution = compiled.resolve(
        &[Keystroke::parse("ctrl-alt-s").expect("parse key")],
        &terminal_context_stack(),
        false,
        false,
        false,
    );
    assert_eq!(resolution.action, Some(KeybindingAction::TerminalClosePane));
}

/// 同作用范围的重复绑定给出冲突诊断，并按 GPUI 约定让后声明项优先。
#[test]
fn duplicate_binding_reports_conflict_and_last_row_wins() {
    let preferences = [
        preference(
            "settings.open",
            "Ctrl+Alt+S",
            "",
            "",
            KeybindingScope::Global,
        ),
        preference(
            "settings.keybindings",
            "Ctrl+Alt+S",
            "",
            "",
            KeybindingScope::Global,
        ),
    ];
    let compiled = CompiledKeybindings::compile(&preferences, KeybindingPlatform::Windows);
    assert!(
        compiled
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.kind == KeybindingDiagnosticKind::Conflict)
    );
    assert_eq!(
        compiled
            .resolve(
                &[Keystroke::parse("ctrl-alt-s").expect("parse key")],
                &terminal_context_stack(),
                false,
                false,
                false,
            )
            .action,
        Some(KeybindingAction::SettingsKeybindings)
    );
}

/// 未知 action 被保留为诊断，编译后的 keymap 不会把它分发成业务动作。
#[test]
fn unknown_action_is_diagnosed_and_never_executed() {
    let unknown = preference(
        "future.action",
        "Ctrl+Alt+S",
        "",
        "",
        KeybindingScope::Global,
    );
    let compiled = CompiledKeybindings::compile(&[unknown], KeybindingPlatform::Windows);
    let resolution = compiled.resolve(
        &[Keystroke::parse("ctrl-alt-s").expect("parse shortcut")],
        &terminal_context_stack(),
        false,
        false,
        false,
    );
    assert_eq!(resolution.action, None);
    assert!(!resolution.pending);
    assert!(compiled.diagnostics().iter().any(|diagnostic| {
        diagnostic.kind == KeybindingDiagnosticKind::UnknownAction
            && diagnostic.action == "future.action"
    }));
}

/// 重复按下、文本焦点和 GPUI IME 状态都会在业务动作前被屏蔽。
#[test]
fn input_guards_do_not_steal_text_or_ime_events() {
    let settings_open = preference(
        "settings.open",
        "Ctrl+Alt+S",
        "",
        "",
        KeybindingScope::Global,
    );
    let compiled = CompiledKeybindings::compile(&[settings_open], KeybindingPlatform::Windows);
    let key = [Keystroke::parse("ctrl-alt-s").expect("parse key")];
    let contexts = terminal_context_stack();

    assert_eq!(
        compiled.resolve(&key, &contexts, true, false, false).action,
        None
    );
    assert_eq!(
        compiled.resolve(&key, &contexts, false, true, false).action,
        None
    );
    assert_eq!(
        compiled.resolve(&key, &contexts, false, false, true).action,
        None
    );

    let mut text_input = KeyContext::default();
    text_input.add(KeybindingContext::TextInput.as_gpui_identifier());
    let mut text_contexts = contexts;
    text_contexts.push(text_input);
    assert_eq!(
        compiled
            .resolve(&key, &text_contexts, false, false, false)
            .action,
        None
    );

    let plain_binding = preference("settings.open", "S", "", "", KeybindingScope::Global);
    let plain = CompiledKeybindings::compile(&[plain_binding], KeybindingPlatform::Windows);
    assert_eq!(
        plain
            .resolve(
                &[Keystroke::parse("s").expect("parse plain key")],
                &terminal_context_stack(),
                false,
                false,
                false,
            )
            .action,
        None
    );
}

/// 无效 GPUI 语法只产生诊断，不会导致应用启动或其他绑定失效。
#[test]
fn malformed_binding_is_reported_without_panicking() {
    let malformed = preference(
        "settings.open",
        "Ctrl+Alt+S+T",
        "",
        "",
        KeybindingScope::Global,
    );
    let compiled = CompiledKeybindings::compile(&[malformed], KeybindingPlatform::Windows);
    assert!(
        compiled
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.kind == KeybindingDiagnosticKind::InvalidBinding)
    );
    let resolution = compiled.resolve(
        &[Keystroke::parse("ctrl-alt-s").expect("parse shortcut")],
        &terminal_context_stack(),
        false,
        false,
        false,
    );
    assert_eq!(resolution.action, None);
    assert!(!resolution.pending);
}
