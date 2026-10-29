//! Agent 归档确认 owner、busy 门禁与异步回执归属。
//!
//! @author kongweiguang

use super::{
    AgentArchiveConfirmationOwner, archive_confirmation_can_submit,
    archive_confirmation_should_close,
};
use kerminal_application::agents::AgentHistoryMode;
use kerminal_domain::{agents::AgentSessionScope, identity::AgentSessionId};

/// 固定同一会话的其他上下文，仅通过 generation 区分先后打开的确认框。
///
/// @author kongweiguang
fn owner(generation: u64) -> AgentArchiveConfirmationOwner {
    AgentArchiveConfirmationOwner {
        id: AgentSessionId::new(),
        expected_title: "Archive fixture".to_owned(),
        mode: AgentHistoryMode::All,
        scope: AgentSessionScope::Global,
        candidate: None,
        generation,
    }
}

/// 同 owner 的普通失败仍可显式重试，提交中的重复点击由共享 busy 状态拒绝。
///
/// @author kongweiguang
#[test]
fn archive_confirmation_retries_failures_but_blocks_while_busy() {
    let owner = owner(1);
    let mut current = Some(owner.clone());

    let close_after_failure = archive_confirmation_should_close(current.as_ref(), &owner, None);
    if close_after_failure {
        current = None;
    }

    assert!(!close_after_failure);
    assert_eq!(current.as_ref(), Some(&owner));
    assert!(!archive_confirmation_can_submit(
        current.as_ref(),
        &owner,
        true
    ));
    assert!(archive_confirmation_can_submit(
        current.as_ref(),
        &owner,
        false
    ));
}

/// 只有当前 generation 对同一会话收到 Archived 回执时才允许关闭确认框。
///
/// @author kongweiguang
#[test]
fn archive_confirmation_closes_only_for_matching_archived_receipt() {
    let owner = owner(4);
    let other_id = AgentSessionId::new();

    assert!(archive_confirmation_should_close(
        Some(&owner),
        &owner,
        Some((&owner.id, true)),
    ));
    assert!(!archive_confirmation_should_close(
        Some(&owner),
        &owner,
        None
    ));
    assert!(!archive_confirmation_should_close(
        Some(&owner),
        &owner,
        Some((&owner.id, false)),
    ));
    assert!(!archive_confirmation_should_close(
        Some(&owner),
        &owner,
        Some((&other_id, true)),
    ));
}

/// 迟到的旧 generation 回执不能关闭后来打开的同一会话确认框。
///
/// @author kongweiguang
#[test]
fn stale_confirmation_generation_cannot_close_a_new_dialog() {
    let submitted = owner(7);
    let mut current = submitted.clone();
    current.generation = 8;

    assert!(!archive_confirmation_can_submit(
        Some(&current),
        &submitted,
        false,
    ));
    assert!(!archive_confirmation_should_close(
        Some(&current),
        &submitted,
        Some((&submitted.id, true)),
    ));
}
