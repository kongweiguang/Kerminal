//! 交互式广播的目标边界与生命周期投影测试。
//!
//! @author kongweiguang

use super::{
    BroadcastTargetAvailability, MAX_BROADCAST_TARGETS, classify_target, project_target_checks,
    validate_target_ids,
};
use kerminal_domain::{
    identity::SessionId,
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use std::{collections::BTreeMap, ffi::OsString};

/// 拒绝空广播集合，避免调用方把“没有目标”误认为已启用。
#[test]
fn rejects_empty_target_set() {
    assert!(validate_target_ids(&[]).is_err());
}

/// 拒绝重复目标，确保一条按序输入不会被同一会话写入两次。
#[test]
fn rejects_duplicate_target_ids() {
    let session_id = SessionId::parse("session-a").expect("valid fixture id");
    assert!(validate_target_ids(&[session_id.clone(), session_id]).is_err());
}

/// 同步固定目标预算，避免 UI 改动后 application 意外接受大规模扇出。
#[test]
fn rejects_target_sets_above_the_shared_limit() {
    let session_ids = (0..=MAX_BROADCAST_TARGETS)
        .map(|index| SessionId::parse(&format!("session-{index}")).expect("valid fixture id"))
        .collect::<Vec<_>>();
    assert!(validate_target_ids(&session_ids).is_err());
}

/// 只允许 Running 会话启动广播，并让关闭中目标保留可诊断的生命周期。
#[test]
fn classifies_running_and_closing_targets_without_rebinding() {
    let running_id = SessionId::parse("session-running").expect("valid fixture id");
    let closing_id = SessionId::parse("session-closing").expect("valid fixture id");
    let missing_id = SessionId::parse("session-missing").expect("valid fixture id");
    let sessions = vec![
        session(running_id.clone(), SessionLifecycle::Running),
        session(closing_id.clone(), SessionLifecycle::Closing),
    ];

    assert_eq!(
        classify_target(&running_id, &sessions),
        BroadcastTargetAvailability::Ready
    );
    assert_eq!(
        classify_target(&closing_id, &sessions),
        BroadcastTargetAvailability::NotRunning(SessionLifecycle::Closing)
    );
    assert_eq!(
        classify_target(&missing_id, &sessions),
        BroadcastTargetAvailability::Missing
    );
}

/// 保持用户选择顺序，避免应用层按 registry 顺序重排明确广播目标。
#[test]
fn target_projection_preserves_the_requested_order() {
    let first_id = SessionId::parse("session-first").expect("valid fixture id");
    let second_id = SessionId::parse("session-second").expect("valid fixture id");
    let live_sessions = vec![
        session(second_id.clone(), SessionLifecycle::Running),
        session(first_id.clone(), SessionLifecycle::Running),
    ];

    let checks = project_target_checks(&[first_id.clone(), second_id.clone()], &live_sessions);

    assert_eq!(checks[0].session_id, first_id);
    assert_eq!(checks[1].session_id, second_id);
}

/// 构造最小真实领域快照，测试只关注 SessionId 与 lifecycle 的广播约束。
fn session(session_id: SessionId, lifecycle: SessionLifecycle) -> SessionInfo {
    SessionInfo {
        session_id,
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("pwsh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::default(),
        lifecycle,
        exit_status: None,
    }
}
