//! 设置 watcher 读回与草稿 revision 的状态决策回归。
//!
//! @author kongweiguang

use super::{
    PreferencesPanel, SettingsRevisionAction, merge_json_value, merge_settings_snapshot,
    unknown_settings_readback_matches,
};
use crate::features::preferences::{ConfigDraft, DraftSnapshot, DraftStatus};
use kerminal_application::preferences::SettingsSnapshot;
use kerminal_domain::preferences::ConfigRevision;

/// 同 revision 自通知不覆盖后续编辑；新 revision 则保留草稿并进入显式冲突。
///
/// @author kongweiguang
#[test]
fn settings_watcher_echo_preserves_later_draft_and_external_revision_conflicts() {
    let saved_revision = ConfigRevision::from_sha256("saved-light".to_owned());
    let external_revision = ConfigRevision::from_sha256("external-write".to_owned());
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "light".to_owned(),
        revision: Some(saved_revision.clone()),
    });
    draft.replace_value("system".to_owned()).unwrap();

    let echo = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&saved_revision),
    );
    assert_eq!(echo, SettingsRevisionAction::SelfWriteEcho);
    assert_eq!(draft.status(), DraftStatus::Ready);
    assert_eq!(draft.value(), "system");
    assert!(draft.is_dirty());

    let external = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&external_revision),
    );
    assert_eq!(external, SettingsRevisionAction::ExternalConflict);
    assert!(draft.mark_external_change());
    assert_eq!(draft.status(), DraftStatus::ExternalConflict);
    assert_eq!(draft.value(), "system");
    assert!(draft.is_dirty());
}

/// Saving、未知回执与只读状态不能由 watcher 当作 Ready/可覆盖草稿。
///
/// @author kongweiguang
#[test]
fn settings_watcher_preserves_inflight_unknown_and_read_only_snapshots() {
    let base_revision = ConfigRevision::from_sha256("base".to_owned());
    let changed_revision = ConfigRevision::from_sha256("later".to_owned());
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "base".to_owned(),
        revision: Some(base_revision.clone()),
    });
    draft.replace_value("local".to_owned()).unwrap();
    draft.begin_save().unwrap();

    let saving = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&changed_revision),
    );
    assert_eq!(saving, SettingsRevisionAction::Preserve);
    assert_eq!(draft.status(), DraftStatus::Saving);
    draft.save_outcome_unknown().unwrap();

    let unknown = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&changed_revision),
    );
    assert_eq!(unknown, SettingsRevisionAction::Preserve);
    assert_eq!(draft.status(), DraftStatus::OutcomeUnknown);
    assert_eq!(draft.value(), "local");

    let readonly = PreferencesPanel::settings_revision_action(
        Some(DraftStatus::Ready),
        false,
        true,
        Some(&base_revision),
        Some(&changed_revision),
    );
    assert_eq!(readonly, SettingsRevisionAction::ReadOnlyChanged);
}

/// 三方合并吸收未编辑远端字段，保留本地字段以及双方并发修改的数组字段。
#[test]
fn settings_three_way_merge_rebases_remote_fields_and_keeps_local_conflicts() {
    let baseline = serde_json::json!({
        "theme": "dark",
        "terminal": {"font": "A", "size": 14},
        "rules": [{"pattern": "old"}]
    });
    let local = serde_json::json!({
        "theme": "dark",
        "terminal": {"font": "B", "size": 14},
        "rules": [{"pattern": "local"}]
    });
    let remote = serde_json::json!({
        "theme": "light",
        "terminal": {"font": "remote", "size": 16},
        "rules": [{"pattern": "remote"}]
    });

    assert_eq!(
        merge_json_value(&baseline, &local, &remote),
        serde_json::json!({
            "theme": "light",
            "terminal": {"font": "B", "size": 16},
            "rules": [{"pattern": "local"}]
        })
    );
}

/// 只有远端变化的数组字段整体刷新，不误保留本地基线内容。
#[test]
fn settings_three_way_merge_takes_remote_for_untouched_arrays() {
    let baseline = serde_json::json!({"rules": ["old"]});
    let local = baseline.clone();
    let remote = serde_json::json!({"rules": ["new"]});

    assert_eq!(merge_json_value(&baseline, &local, &remote), remote);
}

/// typed SettingsSnapshot 经 serde 三方合并后保留字段类型与默认完整结构。
#[test]
fn settings_snapshot_three_way_merge_preserves_independent_typed_edits() {
    let baseline = SettingsSnapshot::default();
    let mut local = baseline.clone();
    local.terminal.font_family = "Cascadia Mono".to_owned();
    let mut remote = baseline.clone();
    remote.theme_mode = crate::features::preferences::ThemeMode::Light;

    let merged = merge_settings_snapshot(&baseline, &local, &remote)
        .expect("typed settings should serialize and deserialize for field merging");
    assert_eq!(merged.theme_mode, remote.theme_mode);
    assert_eq!(merged.terminal.font_family, local.terminal.font_family);
}

/// 未知提交只接受完整快照相等的读回，远端多字段变化不能被当作成功回执。
#[test]
fn unknown_settings_readback_requires_full_snapshot_equality() {
    let submitted = SettingsSnapshot::default();
    assert!(unknown_settings_readback_matches(
        &submitted,
        &submitted.clone()
    ));

    let mut changed = submitted.clone();
    changed.background_enabled = true;
    assert!(!unknown_settings_readback_matches(&submitted, &changed));
}
