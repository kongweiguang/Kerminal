//! Root 分屏渲染适配器的纯状态与身份映射测试。
//!
//! @author kongweiguang

use crate::features::workspace_controller::WorkspaceResourceOperation;
use kerminal_application::workspace::WorkspaceSessionTab;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    workspace::{SplitDirection, TerminalLayoutNode},
};

use super::{
    PaneSurfaceState, ProjectionIndex, ProjectionLookupError, RuntimePanePhase,
    WorkspacePaneProjection, candidate_retry_pane, classify_surface_state,
    close_operation_blocks_retry_or_measurement, close_operation_targets_pane, pane_surface_state,
    workspace_pane_is_closing, workspace_pane_projection_status, workspace_pane_status_banner,
};

/// 测试 ID 使用领域构造器，避免测试 fixture 引入第二套身份格式。
fn pane_id() -> PaneId {
    PaneId::new()
}

/// CloseFailed 同时保留原错误/重试文案，只有仍进行中的 Closing 才显示覆盖提示。
///
/// @author kongweiguang
#[test]
fn close_failed_keeps_failure_banner_and_retry_status() {
    let phase = Some(RuntimePanePhase::CloseFailed);
    let failure: gpui_kit::SharedString = "recorded cleanup failure".into();
    let projected = workspace_pane_projection_status(phase, Some(failure.clone()));

    assert!(!workspace_pane_is_closing(phase));
    assert_eq!(
        workspace_pane_status_banner(projected, false),
        Some(failure)
    );
    assert!(workspace_pane_projection_status(phase, None).is_some());
    assert!(workspace_pane_is_closing(Some(RuntimePanePhase::Closing)));
}

/// 构造包含两个稳定叶子的布局，专门验证投影顺序与布局顺序相互独立。
fn two_pane_layout(first: PaneId, second: PaneId) -> TerminalLayoutNode {
    TerminalLayoutNode::Split {
        id: "test-root-split".to_owned(),
        direction: SplitDirection::Horizontal,
        children: vec![
            TerminalLayoutNode::Pane { pane_id: first },
            TerminalLayoutNode::Pane { pane_id: second },
        ],
        sizes: None,
    }
}

/// 创建携带会话身份但尚无快照的投影；群发默认为隐藏，避免身份测试依赖额外运行态。
///
/// @author kongweiguang
fn projection(pane_id: PaneId, session_id: SessionId) -> WorkspacePaneProjection {
    WorkspacePaneProjection {
        pane_id,
        title: "Terminal".into(),
        session_id: Some(session_id),
        target: None,
        snapshot: None,
        status: None,
        is_closing: false,
        split_candidate_cancelable: false,
        close_operation_pending: false,
        reconnect: None,
        restart: None,
        broadcast: Default::default(),
    }
}

/// 首显前没有 session、解析后 target 或快照是正常状态；Root 真实投影路径不能把 Pending 信息变成 Failed。
///
/// @author kongweiguang
#[test]
fn unadmitted_persisted_pending_projection_reaches_restoring_probe_surface() {
    let mut pending = projection(pane_id(), SessionId::new());
    pending.session_id = None;
    pending.status = workspace_pane_projection_status(Some(RuntimePanePhase::RestorePending), None);

    assert!(pending.target.is_none());
    assert!(pending.snapshot.is_none());
    assert!(pending.status.is_none());
    assert_eq!(pane_surface_state(&pending), PaneSurfaceState::Restoring);
}

/// 仅移除 Pending 的信息兜底；真正的失败仍显示可见错误，不误入首显恢复循环。
///
/// @author kongweiguang
#[test]
fn failed_restore_projection_keeps_error_surface_and_recorded_message() {
    let mut failed = projection(pane_id(), SessionId::new());
    failed.session_id = None;
    failed.status = workspace_pane_projection_status(Some(RuntimePanePhase::RestoreFailed), None);
    assert!(failed.status.is_some());
    assert_eq!(pane_surface_state(&failed), PaneSurfaceState::Failed);

    let error: gpui_kit::SharedString = "recorded restore error".into();
    failed.status = workspace_pane_projection_status(
        Some(RuntimePanePhase::RestoreFailed),
        Some(error.clone()),
    );
    assert_eq!(failed.status, Some(error));
    assert_eq!(pane_surface_state(&failed), PaneSurfaceState::Failed);
}

/// 本地候选尚无 target binding 时，启动失败仍必须保留精确 pane 的显式重试动作。
///
/// @author kongweiguang
#[test]
fn failed_candidate_has_retry_target_without_a_session_target() {
    let candidate_pane = pane_id();
    let mut failed = projection(candidate_pane.clone(), SessionId::new());
    failed.session_id = None;
    failed.status = Some("open failed".into());

    assert!(failed.target.is_none());
    assert_eq!(
        candidate_retry_pane(&failed, &candidate_pane),
        Some(candidate_pane.clone())
    );
    assert_eq!(candidate_retry_pane(&failed, &pane_id()), None);

    failed.is_closing = true;
    assert_eq!(candidate_retry_pane(&failed, &candidate_pane), None);

    failed.is_closing = false;
    failed.close_operation_pending = true;
    assert_eq!(pane_surface_state(&failed), PaneSurfaceState::Failed);
    assert_eq!(candidate_retry_pane(&failed, &candidate_pane), None);
}

/// 同一关闭投影在普通失败面板与候选测量路径都必须拦截，避免两个分支各自暴露重试/探针。
///
/// @author kongweiguang
#[test]
fn close_owned_failure_blocks_both_retry_and_candidate_measurement_paths() {
    let candidate_pane = pane_id();
    let mut failed = projection(candidate_pane.clone(), SessionId::new());
    failed.session_id = None;
    failed.status = Some("restore failed".into());
    failed.close_operation_pending = true;

    assert_eq!(pane_surface_state(&failed), PaneSurfaceState::Failed);
    assert!(close_operation_blocks_retry_or_measurement(&failed, false));
    assert!(close_operation_blocks_retry_or_measurement(&failed, true));
    assert_eq!(candidate_retry_pane(&failed, &candidate_pane), None);

    failed.close_operation_pending = false;
    assert!(!close_operation_blocks_retry_or_measurement(&failed, false));
    assert!(!close_operation_blocks_retry_or_measurement(&failed, true));
    assert_eq!(
        candidate_retry_pane(&failed, &candidate_pane),
        Some(candidate_pane)
    );
}

/// Pane 关闭和批量 Tab 关闭都屏蔽恢复重试，但不影响其它 pane 的入口。
///
/// @author kongweiguang
#[test]
fn pending_close_projection_matches_only_its_resource_targets() {
    let closing_pane = pane_id();
    let unrelated_pane = pane_id();
    let close_pane = WorkspaceResourceOperation::ClosePane {
        operation_id: "close-pane".to_owned(),
        tab_id: TabId::new(),
        pane_id: closing_pane.clone(),
    };
    assert!(close_operation_targets_pane(
        Some(&close_pane),
        &closing_pane
    ));
    assert!(!close_operation_targets_pane(
        Some(&close_pane),
        &unrelated_pane
    ));
    assert!(!close_operation_targets_pane(None, &closing_pane));

    let tab_id = TabId::new();
    let close_tab = WorkspaceResourceOperation::CloseTab {
        operation_id: "close-tab".to_owned(),
        tab_id: tab_id.clone(),
        tab: Box::new(WorkspaceSessionTab::SftpTransfer {
            id: tab_id,
            tab_group_id: None,
            title: "Files".to_owned(),
            machine_id: "local".to_owned(),
            left_host_id: None,
            locked_left_host_id: None,
            right_host_id: None,
            external_launch_id: None,
            initial_left_path: None,
            initial_right_path: None,
            initial_right_selection: None,
        }),
        pane_ids: vec![closing_pane.clone()],
    };
    assert!(close_operation_targets_pane(
        Some(&close_tab),
        &closing_pane
    ));
    assert!(!close_operation_targets_pane(
        Some(&close_tab),
        &unrelated_pane
    ));
}

/// 乱序投影仍按 PaneId 精确映射，缺少时不会把其他 pane 的会话作为默认值。
///
/// @author kongweiguang
#[test]
fn projection_lookup_uses_stable_pane_identity_not_list_order() {
    let first_pane = pane_id();
    let second_pane = pane_id();
    let first_session = SessionId::new();
    let second_session = SessionId::new();
    let layout = two_pane_layout(first_pane.clone(), second_pane.clone());
    let index = ProjectionIndex::build(
        &layout,
        &first_pane,
        vec![
            projection(second_pane.clone(), second_session.clone()),
            projection(first_pane.clone(), first_session.clone()),
        ],
    );

    let Ok(first) = index.lookup(&first_pane) else {
        panic!("第一 pane 应绑定到自身投影");
    };
    let Ok(second) = index.lookup(&second_pane) else {
        panic!("第二 pane 应绑定到自身投影");
    };
    assert_eq!(first.session_id, Some(first_session));
    assert_eq!(second.session_id, Some(second_session));
    assert!(!index.has_issues());
}

/// 重复 pane 投影被整体拒绝，避免任选其中一个 session 造成终端串线。
///
/// @author kongweiguang
#[test]
fn duplicate_projection_is_not_rendered_as_either_session() {
    let first_pane = pane_id();
    let second_pane = pane_id();
    let layout = two_pane_layout(first_pane.clone(), second_pane.clone());
    let index = ProjectionIndex::build(
        &layout,
        &first_pane,
        vec![
            projection(first_pane.clone(), SessionId::new()),
            projection(first_pane.clone(), SessionId::new()),
            projection(second_pane.clone(), SessionId::new()),
        ],
    );

    assert!(matches!(
        index.lookup(&first_pane),
        Err(ProjectionLookupError::Duplicate)
    ));
    assert!(index.lookup(&second_pane).is_ok());
    assert!(index.has_issues());
}

/// 缺失、重复、额外投影或焦点越界都能被标记，不以静默默认值覆盖 Root 状态。
///
/// @author kongweiguang
#[test]
fn projection_index_reports_incomplete_and_unexpected_state() {
    let first_pane = pane_id();
    let second_pane = pane_id();
    let unexpected_pane = pane_id();
    let missing_focus = pane_id();
    let layout = two_pane_layout(first_pane.clone(), second_pane.clone());
    let index = ProjectionIndex::build(
        &layout,
        &missing_focus,
        vec![
            projection(first_pane.clone(), SessionId::new()),
            projection(unexpected_pane.clone(), SessionId::new()),
        ],
    );

    assert!(index.has_issues());
    assert!(index.lookup(&first_pane).is_ok());
    assert!(matches!(
        index.lookup(&second_pane),
        Err(ProjectionLookupError::Missing)
    ));
}

/// 快照必须与该 pane 的 session 一致；迟到的其他会话数据始终保持隐藏。
#[test]
fn surface_state_requires_matching_session_snapshot() {
    let session = SessionId::new();
    let other_session = SessionId::new();

    assert_eq!(
        classify_surface_state(Some(&session), Some(&session), false, false),
        PaneSurfaceState::Ready
    );
    assert_eq!(
        classify_surface_state(Some(&session), Some(&other_session), false, false),
        PaneSurfaceState::SnapshotSessionMismatch
    );
    assert_eq!(
        classify_surface_state(Some(&session), None, false, false),
        PaneSurfaceState::Restoring
    );
    assert_eq!(
        classify_surface_state(None, None, true, false),
        PaneSurfaceState::Failed
    );
    assert_eq!(
        classify_surface_state(Some(&session), Some(&other_session), false, true),
        PaneSurfaceState::SnapshotSessionMismatch
    );
    assert_eq!(
        classify_surface_state(Some(&session), Some(&session), false, true),
        PaneSurfaceState::Closing
    );
}
