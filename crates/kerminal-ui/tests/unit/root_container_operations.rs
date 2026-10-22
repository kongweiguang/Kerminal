//! 容器事件必须同时匹配来源视图、冻结意图和当前终端，防止未重绘的旧菜单转投目标。
//!
//! @author kongweiguang

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{ContainerRuntime, ContainerTarget},
    sessions::SessionOwner,
};

use super::container_context_allows_request;
use crate::features::operations::{ContainerPanelContext, ContainerPanelUnavailableReason};

/// 固定完整 UI owner，使代次、分屏和宿主切换的断言都经过同一生产比较函数。
fn host_context() -> ContainerPanelContext {
    ContainerPanelContext::Host {
        binding: UiSessionBinding {
            session_id: SessionId::new(),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            session_generation: 4,
        },
        host_id: Some(HostId::new()),
    }
}

/// 同一 SSH 主机的重连和不同 pane 也必须重新点击，不能只按 HostId 认定旧意图仍有效。
#[test]
fn stale_menu_cannot_cross_session_generation_or_owner() {
    let original = host_context();
    assert!(container_context_allows_request(
        Some(&original),
        Some(&original),
        &original,
        true
    ));
    let mut changed = original.clone();
    if let ContainerPanelContext::Host { binding, .. } = &mut changed {
        binding.session_generation += 1;
    }
    assert!(!container_context_allows_request(
        Some(&original),
        Some(&original),
        &changed,
        true
    ));
    assert!(!container_context_allows_request(
        Some(&changed),
        Some(&original),
        &changed,
        true
    ));
    let mut changed = original.clone();
    if let ContainerPanelContext::Host { binding, .. } = &mut changed {
        binding.owner = SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        };
    }
    assert!(!container_context_allows_request(
        Some(&original),
        Some(&original),
        &changed,
        true
    ));
}

/// 容器信息允许只读刷新，所有生命周期、Compose、文件和 exec 请求共用宿主门禁。
#[test]
fn container_info_can_read_but_cannot_execute_host_actions() {
    let ContainerPanelContext::Host { binding, host_id } = host_context() else {
        unreachable!()
    };
    let info = ContainerPanelContext::ContainerInfo {
        binding,
        target: ContainerTarget {
            host_id,
            runtime: ContainerRuntime::Podman,
            container_id: "original-container".into(),
        },
        terminal_running: false,
    };
    assert!(container_context_allows_request(
        Some(&info),
        Some(&info),
        &info,
        false
    ));
    assert!(!container_context_allows_request(
        Some(&info),
        Some(&info),
        &info,
        true
    ));
    let mut other = info.clone();
    if let ContainerPanelContext::ContainerInfo { target, .. } = &mut other {
        target.container_id = "another-container".into();
    }
    assert!(!container_context_allows_request(
        Some(&info),
        Some(&info),
        &other,
        false
    ));
}

/// 两份空目标或省略来源不代表授权，避免失败/断线状态被解释成本机请求。
#[test]
fn absent_or_unavailable_context_never_admits_panel_requests() {
    let unavailable = ContainerPanelContext::Unavailable {
        reason: ContainerPanelUnavailableReason::NotReady,
    };
    let host = host_context();
    assert!(!container_context_allows_request(
        Some(&unavailable),
        Some(&unavailable),
        &unavailable,
        false
    ));
    assert!(!container_context_allows_request(
        None,
        Some(&host),
        &host,
        true
    ));
    assert!(!container_context_allows_request(
        Some(&host),
        None,
        &host,
        true
    ));
    assert!(!container_context_allows_request(None, None, &host, true));
}
