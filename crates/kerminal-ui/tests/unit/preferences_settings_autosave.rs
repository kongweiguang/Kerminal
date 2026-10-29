//! 设置 autosave 代次与单飞请求状态机测试。
//!
//! @author kongweiguang

use super::super::PreferencesPanel;
use super::super::changes::{
    SettingsRevisionAction, needs_settings_recovery_readback, should_resave_ready_self_write_echo,
};
use super::{SettingsAutosaveState, SettingsSaveRequest};
use crate::features::preferences::{ConfigDraft, DraftSnapshot, DraftStatus};
use kerminal_domain::preferences::ConfigRevision;

/// 后续编辑使先前计时器失效，过期代次不能创建保存请求。
#[test]
fn later_edit_invalidates_an_older_debounce_generation() {
    let mut state = SettingsAutosaveState::default();
    let first = state.edited();
    let latest = state.edited();

    assert!(!state.is_current(first));
    assert!(state.is_current(latest));
    assert_eq!(state.request_save(first), SettingsSaveRequest::Stale);
}

/// 当前 CAS 未完成时只排队较新编辑，回执后才允许启动下一次写入。
#[test]
fn edits_during_a_save_are_serialized_after_the_current_receipt() {
    let mut state = SettingsAutosaveState::default();
    let first_generation = state.edited();
    let SettingsSaveRequest::Started(first_ticket) = state.request_save(first_generation) else {
        panic!("first edit should start a save");
    };

    let latest_generation = state.edited();
    assert_eq!(
        state.request_save(latest_generation),
        SettingsSaveRequest::Queued
    );
    assert_eq!(
        state.finish_save(first_ticket, true),
        Some(latest_generation)
    );
    assert!(matches!(
        state.request_save(latest_generation),
        SettingsSaveRequest::Started(_)
    ));
}

/// 保存中编辑排队后，watcher 推进 load generation 并读回成功写入时，续存队列中的新值。
///
/// @author kongweiguang
#[test]
fn self_write_echo_resubmits_queued_edits_after_watcher_advanced_generation() {
    let initial_revision = ConfigRevision::from_sha256("initial".to_owned());
    let saved_revision = ConfigRevision::from_sha256("saved".to_owned());
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "base".to_owned(),
        revision: Some(initial_revision),
    });
    draft.replace_value("submitted".to_owned()).unwrap();

    let mut autosave = SettingsAutosaveState::default();
    let submitted_generation = autosave.edited();
    let SettingsSaveRequest::Started(submitted_ticket) =
        autosave.request_save(submitted_generation)
    else {
        panic!("the submitted edit should start a save");
    };
    draft.begin_save().unwrap();

    draft
        .replace_value_during_save("follow-up".to_owned())
        .unwrap();
    let follow_up_generation = autosave.edited();
    assert_eq!(
        autosave.request_save(follow_up_generation),
        SettingsSaveRequest::Queued
    );

    // watcher 在当前 CAS 期间启动读回并推进面板代次。
    let save_generation = 1;
    let watcher_generation = save_generation + 1;
    let reconcile_watcher_change = save_generation != watcher_generation;
    assert!(reconcile_watcher_change);
    assert_eq!(
        PreferencesPanel::settings_revision_action(
            Some(DraftStatus::Saving),
            true,
            false,
            draft.revision().as_ref(),
            Some(&saved_revision),
        ),
        SettingsRevisionAction::Preserve
    );
    assert_eq!(
        PreferencesPanel::settings_revision_action(
            Some(DraftStatus::OutcomeUnknown),
            true,
            false,
            Some(&saved_revision),
            Some(&saved_revision),
        ),
        SettingsRevisionAction::Preserve
    );

    draft
        .save_succeeded(DraftSnapshot {
            value: "submitted".to_owned(),
            revision: Some(saved_revision.clone()),
        })
        .unwrap();
    assert_eq!(draft.status(), DraftStatus::Ready);
    assert_eq!(draft.value(), "follow-up");
    assert!(draft.is_dirty());
    assert_eq!(
        autosave.finish_save(
            submitted_ticket,
            draft.is_dirty() && !reconcile_watcher_change,
        ),
        None
    );

    let echo_action = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&saved_revision),
    );
    assert_eq!(echo_action, SettingsRevisionAction::SelfWriteEcho);
    assert!(should_resave_ready_self_write_echo(
        draft.status(),
        draft.is_dirty()
    ));
    assert!(!should_resave_ready_self_write_echo(
        DraftStatus::OutcomeUnknown,
        true
    ));
    assert!(!should_resave_ready_self_write_echo(
        DraftStatus::ExternalConflict,
        true
    ));

    autosave.invalidate_pending();
    let resumed_generation = autosave.current_generation();
    let SettingsSaveRequest::Started(resumed_ticket) = autosave.request_save(resumed_generation)
    else {
        panic!("the confirmed self-write echo should resume the latest edit");
    };
    draft.begin_save().unwrap();
    assert!(autosave.is_active(resumed_ticket));
    assert_eq!(draft.pending_save_value(), Some(&"follow-up".to_owned()));
    assert_eq!(draft.revision(), &Some(saved_revision));
}

/// 冲突或结果未知关闭续写队列，且旧回执不能完成重载后的新请求。
#[test]
fn blocked_result_drops_queued_work_and_reset_rejects_late_receipts() {
    let mut state = SettingsAutosaveState::default();
    let first_generation = state.edited();
    let SettingsSaveRequest::Started(first_ticket) = state.request_save(first_generation) else {
        panic!("first edit should start a save");
    };
    let queued_generation = state.edited();
    assert_eq!(
        state.request_save(queued_generation),
        SettingsSaveRequest::Queued
    );
    assert_eq!(state.finish_save(first_ticket, false), None);

    let second_generation = state.edited();
    let SettingsSaveRequest::Started(second_ticket) = state.request_save(second_generation) else {
        panic!("new request should start after the blocked result is handled");
    };
    state.reset();
    assert!(!state.is_active(second_ticket));
    assert_eq!(state.finish_save(second_ticket, true), None);
    assert!(!state.is_current(second_generation));
}

/// 回读确认旧提交后，较新编辑用已确认的新 revision 建立新的单飞 CAS。
///
/// @author kongweiguang
#[test]
fn unknown_readback_confirmation_resubmits_preserved_follow_up() {
    let first_revision = ConfigRevision::from_sha256("first".to_owned());
    let confirmed_revision = ConfigRevision::from_sha256("confirmed".to_owned());
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "base".to_owned(),
        revision: Some(first_revision),
    });
    draft.replace_value("submitted".to_owned()).unwrap();

    let mut autosave = SettingsAutosaveState::default();
    let submitted_generation = autosave.edited();
    let SettingsSaveRequest::Started(submitted_ticket) =
        autosave.request_save(submitted_generation)
    else {
        panic!("the first edit should start a save");
    };
    draft.begin_save().unwrap();

    draft
        .replace_value_during_save("follow-up".to_owned())
        .unwrap();
    let follow_up_generation = autosave.edited();
    assert_eq!(
        autosave.request_save(follow_up_generation),
        SettingsSaveRequest::Queued
    );
    draft.save_outcome_unknown().unwrap();
    assert_eq!(autosave.finish_save(submitted_ticket, false), None);

    draft
        .confirm_outcome_unknown(DraftSnapshot {
            value: "submitted".to_owned(),
            revision: Some(confirmed_revision.clone()),
        })
        .unwrap();
    assert_eq!(draft.value(), "follow-up");
    assert!(draft.is_dirty());

    autosave.invalidate_pending();
    let fresh_generation = autosave.current_generation();
    assert!(matches!(
        autosave.request_save(fresh_generation),
        SettingsSaveRequest::Started(_)
    ));
    draft.begin_save().unwrap();
    assert_eq!(draft.pending_save_value(), Some(&"follow-up".to_owned()));
    assert_eq!(draft.revision(), &Some(confirmed_revision));
}

/// 安全 rebase 后的合并值使用远端 revision 发起新 CAS，不复用冲突前的计时器。
///
/// @author kongweiguang
#[test]
fn external_rebase_resubmits_merged_value_with_remote_revision() {
    let base_revision = ConfigRevision::from_sha256("base".to_owned());
    let remote_revision = ConfigRevision::from_sha256("remote".to_owned());
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "base".to_owned(),
        revision: Some(base_revision),
    });
    draft.replace_value("local".to_owned()).unwrap();
    assert!(draft.mark_external_change());

    let mut autosave = SettingsAutosaveState::default();
    let stale_generation = autosave.edited();
    assert!(
        draft
            .rebase(
                DraftSnapshot {
                    value: "remote".to_owned(),
                    revision: Some(remote_revision.clone()),
                },
                "merged".to_owned(),
            )
            .is_ok()
    );
    autosave.invalidate_pending();
    assert!(!autosave.is_current(stale_generation));

    let fresh_generation = autosave.current_generation();
    assert!(matches!(
        autosave.request_save(fresh_generation),
        SettingsSaveRequest::Started(_)
    ));
    draft.begin_save().unwrap();
    assert_eq!(draft.pending_save_value(), Some(&"merged".to_owned()));
    assert_eq!(draft.revision(), &Some(remote_revision));
}

/// 每次重开只重试 unknown/conflict 的读取，普通失败与正常编辑不扩大恢复读回范围。
///
/// @author kongweiguang
#[test]
fn opening_preferences_retries_only_unknown_or_conflicted_readback() {
    assert!(needs_settings_recovery_readback(
        DraftStatus::OutcomeUnknown
    ));
    assert!(needs_settings_recovery_readback(
        DraftStatus::ExternalConflict
    ));
    assert!(!needs_settings_recovery_readback(DraftStatus::Ready));
    assert!(!needs_settings_recovery_readback(DraftStatus::Failed));
    assert!(!needs_settings_recovery_readback(DraftStatus::Saving));
}
