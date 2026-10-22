//! 设置 watcher 读回与草稿 revision 的状态决策回归。
//!
//! @author kongweiguang

use super::{PreferencesPanel, SettingsRevisionAction};
use crate::features::preferences::{ConfigDraft, DraftSnapshot, DraftStatus};
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
