//! 容器操作回执与宿主列表核对的状态回归。
//!
//! @author kongweiguang

use super::{ContainerMutationTracker, ContainerOperation, ContainerOperationOutcome};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ComposeAction, ComposeProjectTarget, ContainerAction, ContainerRuntime, ContainerTarget,
    },
};

/// 固定独立宿主和容器身份，测试不能依赖真实容器运行时。
///
/// @author kongweiguang
fn container_operation(host: &HostId) -> ContainerOperation {
    ContainerOperation::Container {
        target: ContainerTarget {
            host_id: Some(host.clone()),
            runtime: ContainerRuntime::Docker,
            container_id: "fixture-container".to_owned(),
        },
        action: ContainerAction::Stop,
    }
}

/// 没有回执的手动刷新不能提前释放正在执行的写操作。
///
/// @author kongweiguang
#[test]
fn refresh_before_command_receipt_does_not_unlock_operation() {
    let host = HostId::new();
    let operation = container_operation(&host);
    let mut tracker = ContainerMutationTracker::default();
    assert!(tracker.begin(operation.clone()));
    let pre_receipt_read_verifies =
        tracker.needs_refresh(&Some(host.clone()), ContainerRuntime::Docker);
    assert!(!pre_receipt_read_verifies);
    assert!(
        tracker
            .finish_refresh(&Some(host.clone()), ContainerRuntime::Docker, true)
            .is_none()
    );
    assert!(tracker.blocks_scope(&Some(host.clone()), ContainerRuntime::Docker));
    assert!(!tracker.begin(operation));
}

/// 回执前发出的读取即使在回执后返回，也不能作为随后写操作的核对快照。
///
/// @author kongweiguang
#[test]
fn read_started_before_receipt_cannot_verify_later_outcome() {
    let host = HostId::new();
    let operation = container_operation(&host);
    let mut tracker = ContainerMutationTracker::default();
    assert!(tracker.begin(operation.clone()));
    let old_read_verifies = tracker.needs_refresh(&Some(host.clone()), ContainerRuntime::Docker);
    tracker.complete(&operation, ContainerOperationOutcome::Applied);
    if old_read_verifies {
        tracker.finish_refresh(&Some(host.clone()), ContainerRuntime::Docker, true);
    }
    assert!(tracker.blocks_scope(&Some(host.clone()), ContainerRuntime::Docker));
    assert!(tracker.needs_refresh(&Some(host.clone()), ContainerRuntime::Docker));
    assert_eq!(
        tracker.finish_refresh(&Some(host.clone()), ContainerRuntime::Docker, true),
        Some((ContainerOperationOutcome::Applied, true))
    );
    assert!(!tracker.blocks_scope(&Some(host), ContainerRuntime::Docker));
}

/// 未知结果和刷新失败必须保留重试门禁，成功回读后才交由用户判断下一步。
///
/// @author kongweiguang
#[test]
fn unknown_outcome_stays_blocked_until_matching_refresh_succeeds() {
    let host = HostId::new();
    let other_host = HostId::new();
    let operation = container_operation(&host);
    let outcome = ContainerOperationOutcome::Unknown("unknown result".to_owned());
    let mut tracker = ContainerMutationTracker::default();
    assert!(tracker.begin(operation.clone()));
    tracker.complete(&operation, outcome.clone());
    assert!(
        tracker
            .finish_refresh(&Some(other_host), ContainerRuntime::Docker, true)
            .is_none()
    );
    assert_eq!(
        tracker.finish_refresh(&Some(host.clone()), ContainerRuntime::Docker, false),
        Some((outcome.clone(), false))
    );
    assert!(tracker.blocks_scope(&Some(host.clone()), ContainerRuntime::Docker));
    assert!(!tracker.begin(operation));
    assert_eq!(
        tracker.finish_refresh(&Some(host.clone()), ContainerRuntime::Docker, true),
        Some((outcome, true))
    );
    assert!(!tracker.blocks_scope(&Some(host), ContainerRuntime::Docker));
}

/// 同名项目使用完整目录和配置身份展示动画，其他宿主可独立保留待核对操作。
///
/// @author kongweiguang
#[test]
fn compose_feedback_and_busy_scope_keep_full_target_identity() {
    let host = HostId::new();
    let other_host = HostId::new();
    let target = ComposeProjectTarget {
        host_id: Some(host.clone()),
        runtime: ContainerRuntime::Docker,
        project: "demo".to_owned(),
        working_dir: "/srv/one".to_owned(),
        config_files: vec!["compose.yaml".to_owned()],
    };
    let mut other_project = target.clone();
    other_project.working_dir = "/srv/two".to_owned();
    let operation = ContainerOperation::Compose {
        target: target.clone(),
        action: ComposeAction::Down,
    };
    let mut tracker = ContainerMutationTracker::default();
    assert!(tracker.begin(operation.clone()));
    assert!(tracker.compose_loading(&target));
    assert!(tracker.compose_executing(&target));
    assert!(!tracker.compose_loading(&other_project));
    assert!(tracker.begin(container_operation(&other_host)));
    assert!(!tracker.begin(ContainerOperation::Compose {
        target: other_project,
        action: ComposeAction::Up
    }));
    tracker.complete(&operation, ContainerOperationOutcome::Applied);
    assert!(tracker.compose_loading(&target));
    assert!(!tracker.compose_executing(&target));
    assert_eq!(
        tracker.finish_refresh(&Some(host), ContainerRuntime::Docker, true),
        Some((ContainerOperationOutcome::Applied, true))
    );
    assert!(tracker.blocks_scope(&Some(other_host), ContainerRuntime::Docker));
}
