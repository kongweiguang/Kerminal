//! 偏好编辑草稿状态和快捷键规则的独立回归测试。
//!
//! @author kongweiguang

use super::draft::{CloseRequest, ConfigDraft, DraftSnapshot, DraftStatus, DraftTransitionError};
use super::panel::parse_environment;
use super::render::settings_footer_message;
use super::validation::{
    KeybindingDraft, KeyboardPlatform, ProfileIssueKind, SettingsDraft, SettingsIssueKind,
    validate_profile, validate_settings,
};
use super::{InterfaceLanguage, KeybindingScope, ProfileDraft, ThemeMode};
/// 当前读回错误优先于之前成功提示，避免 footer 把真实冲突隐藏在旧状态下面。
///
/// @author kongweiguang
#[test]
fn settings_footer_prefers_current_readback_error_over_stale_status() {
    assert_eq!(
        settings_footer_message(
            Some("Settings changed outside the app. Reload before saving."),
            Some("Settings saved."),
        ),
        Some((
            "Settings changed outside the app. Reload before saving.",
            true,
        )),
    );
    assert_eq!(
        settings_footer_message(None, Some("Settings reloaded.")),
        Some(("Settings reloaded.", false)),
    );
}

/// 构造最小可用的表单快照，保留其余字段默认值且避免先默认后覆盖。
///
/// @author kongweiguang
fn settings_draft() -> SettingsDraft {
    SettingsDraft {
        theme_mode: ThemeMode::System,
        interface_language: InterfaceLanguage::ZhCn,
        ..SettingsDraft::default()
    }
}

/// 构造可直接比较的按键条目，避免各测试独立伪造作用域语义。
fn keybinding(action: &str, scope: KeybindingScope, windows: &str, mac: &str) -> KeybindingDraft {
    KeybindingDraft {
        action: action.to_string(),
        label: action.to_string(),
        description: String::new(),
        binding: windows.to_string(),
        windows_binding: windows.to_string(),
        windows_alternate_binding: None,
        linux_binding: None,
        mac_binding: mac.to_string(),
        scope,
        editable: true,
    }
}

/// 关闭提示取消只收起弹框，确认丢弃才恢复打开时的快照。
#[test]
fn dirty_close_cancel_preserves_draft_and_discard_resets_it() {
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: settings_draft(),
        revision: 4_u64,
    });
    let mut edited = draft.value().clone();
    edited.theme_mode = ThemeMode::Light;
    draft.replace_value(edited).unwrap();

    assert_eq!(draft.request_close(), CloseRequest::ConfirmDiscard);
    assert!(draft.close_confirmation_open());
    draft.cancel_close().unwrap();
    assert!(draft.is_dirty());
    assert_eq!(draft.value().theme_mode, ThemeMode::Light);

    assert_eq!(draft.request_close(), CloseRequest::ConfirmDiscard);
    draft.discard_and_close().unwrap();
    assert!(!draft.is_dirty());
    assert_eq!(draft.value().theme_mode, ThemeMode::System);
}

/// CAS 冲突保留编辑值，并要求重载后才允许下一次保存。
#[test]
fn save_conflict_keeps_draft_and_requires_reload_before_another_save() {
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "initial".to_string(),
        revision: 1_u64,
    });
    draft.replace_value("local edit".to_string()).unwrap();
    draft.begin_save().unwrap();
    draft.save_conflicted().unwrap();

    assert_eq!(draft.status(), DraftStatus::ExternalConflict);
    assert_eq!(draft.value(), "local edit");
    assert_eq!(
        draft.begin_save(),
        Err(DraftTransitionError::ReloadRequired)
    );

    draft
        .reload(DraftSnapshot {
            value: "external edit".to_string(),
            revision: 2,
        })
        .unwrap();
    assert_eq!(draft.value(), "external edit");
    assert_eq!(*draft.revision(), 2);
    assert!(!draft.is_dirty());
}

/// 冲突草稿可被明确丢弃，但仍须重载后才接受新编辑以更新 CAS 版本。
#[test]
fn discarding_conflicted_draft_does_not_clear_reload_requirement() {
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "initial".to_string(),
        revision: 1_u64,
    });
    draft.replace_value("local edit".to_string()).unwrap();
    draft.begin_save().unwrap();
    draft.save_conflicted().unwrap();
    draft.discard_changes().unwrap();

    assert!(!draft.is_dirty());
    assert_eq!(draft.status(), DraftStatus::ExternalConflict);
    assert_eq!(
        draft.replace_value("new edit".to_string()),
        Err(DraftTransitionError::ReloadRequired)
    );
}

/// 普通失败允许重试，只有服务端成功快照能更新 revision。
#[test]
fn failed_save_keeps_editable_draft_and_only_success_updates_revision() {
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: 10_u8,
        revision: 3_u64,
    });
    draft.replace_value(12).unwrap();
    draft.begin_save().unwrap();
    draft.save_failed().unwrap();
    assert_eq!(draft.status(), DraftStatus::Failed);
    assert!(draft.is_dirty());
    draft.begin_save().unwrap();
    draft
        .save_succeeded(DraftSnapshot {
            value: 12,
            revision: 4,
        })
        .unwrap();
    assert_eq!(*draft.revision(), 4);
    assert!(!draft.is_dirty());
}

/// 未知提交结果必须读回才能恢复编辑，禁止再次提交、丢弃或关闭页面。
#[test]
fn outcome_unknown_requires_explicit_readback_before_any_followup_action() {
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "initial".to_string(),
        revision: 1_u64,
    });
    draft.replace_value("local edit".to_string()).unwrap();
    draft.begin_save().unwrap();
    draft.save_outcome_unknown().unwrap();

    assert_eq!(draft.status(), DraftStatus::OutcomeUnknown);
    assert_eq!(draft.value(), "local edit");
    assert_eq!(
        draft.begin_save(),
        Err(DraftTransitionError::ReadBackRequired)
    );
    assert_eq!(
        draft.replace_value("retry without readback".to_string()),
        Err(DraftTransitionError::ReadBackRequired)
    );
    assert_eq!(
        draft.discard_changes(),
        Err(DraftTransitionError::ReadBackRequired)
    );
    assert_eq!(draft.request_close(), CloseRequest::WaitForSave);

    draft
        .reload(DraftSnapshot {
            value: "committed or old value".to_string(),
            revision: 2,
        })
        .unwrap();
    assert_eq!(draft.status(), DraftStatus::Ready);
    assert_eq!(draft.value(), "committed or old value");
    assert_eq!(*draft.revision(), 2);
}

/// 外部文件变更保留 dirty 输入并阻止保存，避免 watcher 静默覆盖用户草稿。
#[test]
fn external_change_marks_editable_dirty_drafts_as_conflicted() {
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: String::from("baseline"),
        revision: 7_u64,
    });
    draft.replace_value(String::from("local edit")).unwrap();

    assert!(draft.mark_external_change());
    assert_eq!(draft.status(), DraftStatus::ExternalConflict);
    assert_eq!(draft.value(), "local edit");
    assert_eq!(draft.revision(), &7);
    assert!(!draft.mark_external_change());
}

/// 字体范围和 modifier 顺序归一化后的全局快捷键冲突都能被发现。
#[test]
fn settings_validation_reports_font_limits_and_shortcut_collisions() {
    let mut settings = settings_draft();
    settings.terminal.font_size = 25;
    settings.keybindings = vec![
        keybinding(
            "settings.open",
            KeybindingScope::Global,
            "Ctrl+Shift+P",
            "Cmd+P",
        ),
        keybinding(
            "terminal.closeTab",
            KeybindingScope::Terminal,
            "Shift+Control+P",
            "Cmd+Q",
        ),
    ];

    let issues = validate_settings(&settings, KeyboardPlatform::Windows);
    assert!(
        issues
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::InvalidFontSize)
    );
    let collision = issues
        .iter()
        .find(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
        .expect("global shortcut must conflict with a terminal shortcut");
    assert_eq!(collision.actions, ["terminal.closeTab", "settings.open"]);
}

/// 背景材质透明度沿用旧配置的 35–100 可调范围，边界可保存而范围外值必须先修正。
#[test]
fn window_opacity_validation_accepts_supported_bounds_and_rejects_outside() {
    let mut settings = settings_draft();

    for value in [35, 100] {
        settings.window_opacity = value;
        assert!(
            !validate_settings(&settings, KeyboardPlatform::Windows)
                .iter()
                .any(|issue| issue.kind == SettingsIssueKind::InvalidWindowOpacity)
        );
    }

    for value in [34, 101] {
        settings.window_opacity = value;
        assert!(
            validate_settings(&settings, KeyboardPlatform::Windows)
                .iter()
                .any(|issue| issue.kind == SettingsIssueKind::InvalidWindowOpacity)
        );
    }
}

/// Workspace 与 Terminal context 会同时出现在 GPUI context stack，编译器必须报告冲突。
#[test]
fn nested_scopes_report_shadowing_and_mac_uses_its_own_binding() {
    let mut settings = settings_draft();
    settings.keybindings = vec![
        keybinding(
            "terminal.closeTab",
            KeybindingScope::Terminal,
            "Ctrl+P",
            "Cmd+P",
        ),
        keybinding(
            "terminal.newTab",
            KeybindingScope::Workspace,
            "Ctrl+P",
            "Cmd+Q",
        ),
    ];

    let windows = validate_settings(&settings, KeyboardPlatform::Windows);
    assert!(
        windows
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );
    let mac = validate_settings(&settings, KeyboardPlatform::Mac);
    assert!(
        !mac.iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );
}

/// 多笔画快捷键由 GPUI 解析器支持，单笔画包含多个普通键则报告真实解析错误。
#[test]
fn keybinding_validation_uses_gpui_chords_and_parser_errors() {
    let mut settings = settings_draft();
    settings.keybindings = vec![keybinding(
        "settings.open",
        KeybindingScope::Global,
        "Ctrl+K Ctrl+C",
        "Cmd+K Cmd+C",
    )];
    assert!(
        !validate_settings(&settings, KeyboardPlatform::Windows)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::InvalidKeybinding)
    );

    settings.keybindings[0].windows_binding = "Ctrl+Alt+S+T".to_owned();
    let issues = validate_settings(&settings, KeyboardPlatform::Windows);
    assert!(
        issues
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::InvalidKeybinding)
    );
}

/// 三平台绑定按各自字段校验；旧 Linux 缺省沿用 Windows 值，显式空值则关闭该平台绑定。
///
/// @author kongweiguang
#[test]
fn platform_shortcut_fields_keep_legacy_fallback_and_explicit_disable() {
    let mut settings = settings_draft();
    let mut paste = keybinding(
        "terminal.paste",
        KeybindingScope::Terminal,
        "Ctrl+V",
        "Cmd+V",
    );
    paste.binding = "Ctrl+Shift+V".to_owned();
    paste.windows_alternate_binding = Some("Ctrl+Shift+V".to_owned());
    let mut close_tab = keybinding(
        "terminal.closeTab",
        KeybindingScope::Workspace,
        "Ctrl+Shift+V",
        "Cmd+W",
    );
    close_tab.linux_binding = Some("Ctrl+V".to_owned());
    settings.keybindings = vec![paste.clone(), close_tab];

    assert!(
        validate_settings(&settings, KeyboardPlatform::Windows)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );
    assert!(
        validate_settings(&settings, KeyboardPlatform::Linux)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );
    assert!(
        !validate_settings(&settings, KeyboardPlatform::Mac)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );

    settings.keybindings[0].windows_alternate_binding = Some(String::new());
    settings.keybindings[0].linux_binding = Some(String::new());
    assert!(
        !validate_settings(&settings, KeyboardPlatform::Windows)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );
    assert!(
        !validate_settings(&settings, KeyboardPlatform::Linux)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );

    // windowsBinding 为空时，旧 Linux 仍回退到通用 binding，而不是被视为无绑定。
    settings.keybindings[0].linux_binding = None;
    settings.keybindings[0].windows_binding.clear();
    settings.keybindings[1].linux_binding = Some("Ctrl+Shift+V".to_owned());
    assert!(
        validate_settings(&settings, KeyboardPlatform::Linux)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::KeybindingCollision)
    );
}

/// Linux 与 Windows 备用键选项仍遵守已有 128 字节上限，避免绕过平台键位校验。
///
/// @author kongweiguang
#[test]
fn optional_platform_shortcut_bindings_obey_length_limit() {
    let mut settings = settings_draft();
    let mut paste = keybinding(
        "terminal.paste",
        KeybindingScope::Terminal,
        "Ctrl+V",
        "Cmd+V",
    );
    paste.windows_alternate_binding = Some("x".repeat(129));
    settings.keybindings = vec![paste];

    assert!(
        validate_settings(&settings, KeyboardPlatform::Windows)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::InvalidKeybinding)
    );

    settings.keybindings[0].windows_alternate_binding = None;
    settings.keybindings[0].linux_binding = Some("x".repeat(129));
    assert!(
        validate_settings(&settings, KeyboardPlatform::Linux)
            .iter()
            .any(|issue| issue.kind == SettingsIssueKind::InvalidKeybinding)
    );
}

/// 修改其它设置时保留旧 Linux 缺省与显式空绑定，避免草稿重建时抹掉兼容语义。
///
/// @author kongweiguang
#[test]
fn shortcut_option_states_survive_unrelated_settings_draft_edit() {
    let mut settings = settings_draft();
    let mut paste = keybinding(
        "terminal.paste",
        KeybindingScope::Terminal,
        "Ctrl+V",
        "Cmd+V",
    );
    paste.windows_alternate_binding = Some(String::new());
    paste.linux_binding = Some(String::new());
    let legacy = keybinding(
        "terminal.closeTab",
        KeybindingScope::Workspace,
        "Ctrl+F4",
        "Cmd+W",
    );
    settings.keybindings = vec![paste, legacy];
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: settings,
        revision: 1_u64,
    });

    let mut updated = draft.value().clone();
    updated.theme_mode = ThemeMode::Light;
    draft.replace_value(updated).unwrap();

    let paste = &draft.value().keybindings[0];
    assert_eq!(paste.windows_alternate_binding.as_deref(), Some(""));
    assert_eq!(paste.linux_binding.as_deref(), Some(""));
    let legacy = &draft.value().keybindings[1];
    assert_eq!(legacy.windows_alternate_binding, None);
    assert_eq!(legacy.linux_binding, None);
}

/// 未知 action 与精确 GPUI 诊断一起显示，但保留兼容配置且不阻止其它设置保存。
#[test]
fn unknown_keybinding_action_is_visible_but_non_blocking() {
    let mut settings = settings_draft();
    settings.keybindings = vec![keybinding(
        "future.action",
        KeybindingScope::Global,
        "Ctrl+Alt+S",
        "Cmd+Alt+S",
    )];

    let issues = validate_settings(&settings, KeyboardPlatform::Windows);
    let diagnostic = issues
        .iter()
        .find(|issue| issue.kind == SettingsIssueKind::UnknownKeybindingAction)
        .expect("unknown action should be diagnosed");
    assert!(diagnostic.message.contains("future.action"));
    assert!(!diagnostic.blocks_save());
}

/// Profile 校验拒绝不能启动的空 shell 和不可表示的环境变量，同时允许参数空格。
#[test]
fn profile_validation_checks_launch_fields_without_reparsing_arguments() {
    let mut profile = ProfileDraft {
        id: None,
        name: "开发 Shell".to_string(),
        shell: "pwsh".to_string(),
        args: vec!["-NoLogo -NoExit".to_string()],
        cwd: Some("C:\\work space".to_string()),
        env: Default::default(),
        sidebar_group_id: None,
        sort_order: 0,
    };
    assert!(validate_profile(&profile).is_empty());

    profile.shell.clear();
    profile
        .env
        .insert("BAD=NAME".to_string(), "value".to_string());
    let issues = validate_profile(&profile);
    assert!(
        issues
            .iter()
            .any(|issue| issue.kind == ProfileIssueKind::InvalidShell)
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue.kind == ProfileIssueKind::InvalidProcessValue)
    );
}

/// 环境变量编辑按首个等号分隔，允许空值和包含等号的 value，并拒绝歧义行。
#[test]
fn profile_environment_parser_preserves_equals_and_rejects_duplicates() {
    let parsed = parse_environment("TOKEN=a=b\r\nEMPTY=\r\n\r\n").unwrap();
    assert_eq!(parsed.get("TOKEN").map(String::as_str), Some("a=b"));
    assert_eq!(parsed.get("EMPTY").map(String::as_str), Some(""));

    assert!(parse_environment("A=first\nA=second").is_err());
    assert!(parse_environment("MISSING_SEPARATOR").is_err());
    assert!(parse_environment("=EMPTY_KEY").is_err());
    assert!(parse_environment("A=contains\0nul").is_err());
}
