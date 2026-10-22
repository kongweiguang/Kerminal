//! Vault UI 的状态门禁测试。
//!
//! @author kongweiguang

use kerminal_application::connections::credentials::VaultKeyAvailability;

use super::vault_action_policy;

/// 未知结果清空 availability 后，显式刷新前不得保留任何写入口。
#[test]
fn unknown_result_without_refreshed_state_has_no_write_actions() {
    let policy = vault_action_policy(None, false);

    assert!(!policy.ensure);
    assert!(!policy.create);
    assert!(!policy.import);
    assert!(!policy.export);
    assert!(!policy.restore);
    assert!(!policy.rotate);
}

/// 恢复状态只允许用户提供并重新验证 key 与加密数据配对。
#[test]
fn recovery_required_allows_only_pair_restore() {
    let availability = VaultKeyAvailability::RecoveryRequired {
        reason: kerminal_application::connections::credentials::VaultRecoveryReason::KeyMismatch,
    };
    let policy = vault_action_policy(Some(&availability), false);

    assert!(!policy.ensure);
    assert!(!policy.create);
    assert!(!policy.import);
    assert!(!policy.export);
    assert!(policy.restore);
    assert!(!policy.rotate);
}

/// 缺少现有 Vault 密钥时只能导入原密钥或显式恢复一组成对文件。
#[test]
fn missing_key_allows_import_or_pair_restore() {
    let policy = vault_action_policy(Some(&VaultKeyAvailability::MissingForExistingVault), false);

    assert!(!policy.ensure);
    assert!(!policy.create);
    assert!(policy.import);
    assert!(!policy.export);
    assert!(policy.restore);
    assert!(!policy.rotate);
}

/// 未初始化状态只展示创建入口，避免把空仓库误当成可导入或可轮换状态。
#[test]
fn uninitialized_vault_allows_ensure_and_create_only() {
    let policy = vault_action_policy(Some(&VaultKeyAvailability::NotInitialized), false);

    assert!(policy.ensure);
    assert!(policy.create);
    assert!(!policy.import);
    assert!(!policy.export);
    assert!(!policy.restore);
    assert!(!policy.rotate);
}

/// 可用密钥才开放备份、导入和轮换，配置只读恢复会覆盖所有写策略。
#[test]
fn ready_key_allows_management_but_read_only_recovery_locks_it() {
    let availability = VaultKeyAvailability::Ready {
        key_id: "test-key".to_owned(),
    };
    let writable = vault_action_policy(Some(&availability), false);
    assert!(writable.ensure);
    assert!(writable.import);
    assert!(writable.export);
    assert!(writable.rotate);
    assert!(!writable.create);
    assert!(!writable.restore);

    let read_only = vault_action_policy(Some(&availability), true);
    assert_eq!(read_only, Default::default());
}
