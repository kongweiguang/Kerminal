//! 验证 Vault 写入期间的秘密草稿与重复操作门禁。
//! @author kongweiguang

use super::SecretWriteState;

/// 成功回执只可清空尚未再编辑的输入，并允许之后再次保存。
#[test]
fn secret_write_preserves_edits_and_rejects_overlapping_writes() {
    let state = SecretWriteState::default();
    assert!(state.begin());
    assert!(state.is_pending());
    assert!(!state.begin());
    state.edited();
    assert!(!state.finish());
    assert!(!state.is_pending());
    assert!(state.begin());
    assert!(state.finish());
    assert!(!state.is_pending());
}
