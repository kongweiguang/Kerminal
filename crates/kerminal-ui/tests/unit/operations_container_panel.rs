//! 固定容器面板只读范围与异步回执目标测试。
//!
//! @author kongweiguang

use super::{
    ContainerPanelContext, host_context_matches_target, same_container_info_identity,
    same_read_context_scope,
};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{ContainerRuntime, ContainerTarget},
    sessions::SessionOwner,
};

/// 固定一致的 UI owner，使读请求范围测试能分别改变运行边沿、target 和 binding。
fn binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 7,
    }
}

/// 终端 running 状态改变只刷新提示，同一容器在途读仍应完成且保留快照。
#[test]
fn terminal_running_edge_keeps_same_read_scope() {
    let target = ContainerTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Podman,
        container_id: "container-123".to_owned(),
    };
    let running = ContainerPanelContext::ContainerInfo {
        binding: binding(),
        target: target.clone(),
        terminal_running: true,
    };
    let ended = ContainerPanelContext::ContainerInfo {
        binding: match &running {
            ContainerPanelContext::ContainerInfo { binding, .. } => binding.clone(),
            _ => unreachable!(),
        },
        target: target.clone(),
        terminal_running: false,
    };

    assert!(same_container_info_identity(&running, &ended));
    assert!(same_read_context_scope(Some(&ended), Some(&running)));
    assert!(!same_read_context_scope(Some(&running), None));
}

/// Runtime 与 host 必须同时命中当前宿主上下文，避免旧行被投到新运行时。
#[test]
fn host_context_matches_only_bound_host_and_runtime() {
    let host_id = HostId::new();
    let context = ContainerPanelContext::Host {
        binding: binding(),
        host_id: Some(host_id.clone()),
    };
    let target = ContainerTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-456".to_owned(),
    };

    assert!(host_context_matches_target(
        &context,
        Some(&host_id),
        ContainerRuntime::Docker,
        &target,
    ));
    assert!(!host_context_matches_target(
        &context,
        Some(&host_id),
        ContainerRuntime::Podman,
        &target,
    ));
    assert!(!host_context_matches_target(
        &context,
        None,
        ContainerRuntime::Docker,
        &target,
    ));
}
