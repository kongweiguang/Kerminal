//! 偏好页退出门禁的独立状态测试。
//!
//! @author kongweiguang

use super::{
    CloseGateAction, QuitDeferral, close_gate_action, quit_deferral,
    should_flush_settings_before_close,
};
use crate::features::preferences::{CloseRequest, DraftStatus};

/// 运行中的同步事务会延后退出，并保留是否已请求协作取消的区别。
#[test]
fn sync_quit_gate_reports_running_and_cancellation_wait_states() {
    assert_eq!(
        quit_deferral(false, true, false, false, None, None),
        Some(QuitDeferral::SyncOperation {
            cancellation_requested: false,
        })
    );
    assert_eq!(
        quit_deferral(false, true, true, false, None, None),
        Some(QuitDeferral::SyncOperation {
            cancellation_requested: true,
        })
    );
}

/// Vault 写入不允许被 Root 的窗口关闭路径丢弃。
#[test]
fn vault_and_profile_transactions_defer_quit() {
    assert_eq!(
        quit_deferral(false, false, false, true, None, None),
        Some(QuitDeferral::VaultOperation)
    );
    assert_eq!(
        quit_deferral(true, false, false, false, None, None),
        Some(QuitDeferral::ProfileOperation)
    );
}

/// 配置结果未知要求显式读回，优先于普通保存中提示。
#[test]
fn unknown_write_requires_readback_before_quit() {
    assert_eq!(
        quit_deferral(
            false,
            false,
            false,
            false,
            Some(DraftStatus::OutcomeUnknown),
            Some(DraftStatus::Saving),
        ),
        Some(QuitDeferral::OutcomeUnknown)
    );
    assert_eq!(
        quit_deferral(false, false, false, false, Some(DraftStatus::Saving), None),
        Some(QuitDeferral::Saving)
    );
}

/// 无后台事务且草稿不在保存或未知状态时，退出守卫可继续检查 dirty-close。
#[test]
fn clean_or_dirty_ready_drafts_do_not_get_misclassified_as_busy() {
    assert_eq!(
        quit_deferral(false, false, false, false, Some(DraftStatus::Ready), None),
        None
    );
    assert_eq!(
        quit_deferral(false, false, false, false, Some(DraftStatus::Failed), None),
        None
    );
}

/// MCP 搜索导航与关闭按钮复用相同的 dirty/save 门禁，保存未知时不能越过目标切页。
#[test]
fn navigation_close_gate_waits_confirms_or_proceeds_from_draft_state() {
    assert_eq!(
        close_gate_action(CloseRequest::WaitForSave, CloseRequest::CloseNow, false),
        CloseGateAction::Wait
    );
    assert_eq!(
        close_gate_action(CloseRequest::CloseNow, CloseRequest::ConfirmDiscard, false),
        CloseGateAction::ConfirmDiscard
    );
    assert_eq!(
        close_gate_action(CloseRequest::CloseNow, CloseRequest::CloseNow, true),
        CloseGateAction::ConfirmDiscard
    );
    assert_eq!(
        close_gate_action(CloseRequest::CloseNow, CloseRequest::CloseNow, false),
        CloseGateAction::Proceed
    );
}

/// 普通关闭 flush 有效 dirty 设置，不阻断保存中/未知/无效文本的离开路径。
#[test]
fn settings_close_flushes_only_valid_dirty_snapshots() {
    assert!(should_flush_settings_before_close(
        Some(DraftStatus::Ready),
        true,
        false
    ));
    assert!(should_flush_settings_before_close(
        Some(DraftStatus::Failed),
        true,
        false
    ));
    assert!(!should_flush_settings_before_close(
        Some(DraftStatus::Ready),
        true,
        true
    ));
    assert!(!should_flush_settings_before_close(
        Some(DraftStatus::Saving),
        true,
        false
    ));
    assert!(!should_flush_settings_before_close(
        Some(DraftStatus::OutcomeUnknown),
        true,
        false
    ));
    assert!(!should_flush_settings_before_close(
        Some(DraftStatus::Ready),
        false,
        false
    ));
}
