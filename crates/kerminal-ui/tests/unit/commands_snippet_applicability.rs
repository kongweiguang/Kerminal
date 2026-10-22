//! 片段适用性查询必须绑定定义 revision 和原 UI session generation。
//!
//! @author kongweiguang

use kerminal_application::{commands::SnippetApplicabilityProjection, sessions::UiSessionBinding};
use kerminal_domain::{
    commands::{
        SnippetApplicabilityAssessment, SnippetApplicabilityReason, SnippetApplicabilityStatus,
        SnippetCatalogOrigin, SnippetPreferenceKey,
    },
    identity::{PaneId, SessionId, TabId},
    preferences::ConfigRevision,
    sessions::SessionOwner,
};

use super::{
    ApplicabilityIdentity, SelectedSnippetApplicability, SnippetApplicabilityPhase,
    is_confirmed_incompatible,
};

/// 同一来源键在保存 revision 或重连代次变化后代表不同适用性查询。
///
/// @author kongweiguang
fn identity(revision: &str, session_generation: u64) -> ApplicabilityIdentity {
    ApplicabilityIdentity {
        key: SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::User,
            snippet_id: "inspection".to_owned(),
        },
        revision: Some(ConfigRevision::from_sha256(revision.to_owned())),
        binding: Some(UiSessionBinding {
            session_id: SessionId::parse("session-main").expect("valid session id"),
            owner: SessionOwner::Ui {
                tab_id: TabId::parse("tab-main").expect("valid tab id"),
                pane_id: PaneId::parse("pane-main").expect("valid pane id"),
            },
            session_generation,
        }),
    }
}

/// 查询替换时取消旧票据，旧回执不能覆盖新 revision 与 binding 的等待状态。
///
/// @author kongweiguang
#[test]
fn replacement_cancels_old_query_and_rejects_late_receipt() {
    let mut state = SelectedSnippetApplicability::default();
    let old_identity = identity("revision-1", 4);
    let current_identity = identity("revision-2", 5);
    let old = state.begin(old_identity.clone());
    let current = state.begin(current_identity.clone());

    assert!(old.cancellation.is_cancelled());
    assert!(!state.finish(
        &old,
        Some(current_identity.clone()),
        Ok(projection(SnippetApplicabilityStatus::Compatible, None)),
    ));
    assert_eq!(
        state.phase_for(&current_identity),
        Some(&SnippetApplicabilityPhase::Loading)
    );
    assert!(state.finish(
        &current,
        Some(current_identity.clone()),
        Ok(projection(SnippetApplicabilityStatus::Compatible, None)),
    ));
    assert!(matches!(
        state.phase_for(&current_identity),
        Some(SnippetApplicabilityPhase::Ready(_))
    ));
}

/// 即使未启动替代查询，当前 binding 改变也会让旧目标的迟到评估失效。
///
/// @author kongweiguang
#[test]
fn changed_binding_rejects_receipt_without_replacing_loading_state() {
    let mut state = SelectedSnippetApplicability::default();
    let selected = identity("revision-1", 4);
    let ticket = state.begin(selected.clone());
    let rebound = identity("revision-1", 5);

    assert!(!state.finish(
        &ticket,
        Some(rebound),
        Ok(projection(SnippetApplicabilityStatus::Incompatible, None)),
    ));
    assert_eq!(
        state.phase_for(&selected),
        Some(&SnippetApplicabilityPhase::Loading)
    );
}

/// Unknown 和错误都保留为独立状态；只有可信返回 Incompatible 才禁用运行。
///
/// @author kongweiguang
#[test]
fn pending_unknown_and_failure_do_not_block_but_explicit_mismatch_does() {
    let mut state = SelectedSnippetApplicability::default();
    let selected = identity("revision-1", 4);
    let pending = state.begin(selected.clone());
    assert!(!is_confirmed_incompatible(state.phase_for(&selected)));

    assert!(state.finish(
        &pending,
        Some(selected.clone()),
        Ok(projection(
            SnippetApplicabilityStatus::Unknown,
            Some(SnippetApplicabilityReason::TargetNotAvailable),
        )),
    ));
    assert!(!is_confirmed_incompatible(state.phase_for(&selected)));

    let failed = state.begin(selected.clone());
    assert!(state.finish(
        &failed,
        Some(selected.clone()),
        Err("read failed".to_owned()),
    ));
    assert!(!is_confirmed_incompatible(state.phase_for(&selected)));

    let incompatible = state.begin(selected.clone());
    assert!(state.finish(
        &incompatible,
        Some(selected.clone()),
        Ok(projection(
            SnippetApplicabilityStatus::Incompatible,
            Some(SnippetApplicabilityReason::ShellDialectMismatch),
        )),
    ));
    assert!(is_confirmed_incompatible(state.phase_for(&selected)));
}

/// 选择清除后取消任务并移除旧结论，不能在关闭或切换后复活。
///
/// @author kongweiguang
#[test]
fn clear_cancels_pending_query_and_discards_its_receipt() {
    let mut state = SelectedSnippetApplicability::default();
    let selected = identity("revision-1", 4);
    let ticket = state.begin(selected.clone());

    state.clear();

    assert!(ticket.cancellation.is_cancelled());
    assert!(!state.finish(
        &ticket,
        None,
        Ok(projection(SnippetApplicabilityStatus::Incompatible, None)),
    ));
    assert!(state.phase_for(&selected).is_none());
    assert!(!is_confirmed_incompatible(None));
}

/// 构造与 application 投影一致的纯评估回执，测试不依赖 shell 猜测或 UI host。
///
/// @author kongweiguang
fn projection(
    status: SnippetApplicabilityStatus,
    reason: Option<SnippetApplicabilityReason>,
) -> SnippetApplicabilityProjection {
    SnippetApplicabilityProjection {
        platforms: vec!["linux".to_owned()],
        shells: vec!["bash".to_owned()],
        assessment: SnippetApplicabilityAssessment { status, reason },
    }
}
