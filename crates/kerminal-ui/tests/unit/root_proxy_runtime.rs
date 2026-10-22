//! SOCKS 代理 Root runtime 的取消注册与迟到请求隔离测试。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::EntityId;
use kerminal_domain::identity::{PaneId, TabId};

/// 用户在应用执行器尚未分配 token 前取消时，后续注册必须立即得到已取消 token。
#[test]
fn cancellation_before_token_registration_is_forwarded() {
    let slot = CancellationSlot::default();
    let token = CancellationToken::new();

    slot.cancel();
    slot.install(token.clone());

    assert!(slot.is_cancelled());
    assert!(token.is_cancelled());
}

/// 新 current read 替换旧代次并取消旧 token，旧回执不得再成为当前请求。
#[test]
fn replacing_current_read_invalidates_and_cancels_previous_ticket() {
    let mut runtime = ProxyRuntime::default();
    let old_slot = CancellationSlot::default();
    let panel_id = EntityId::from(7);
    let old_ticket = runtime.begin(
        ProxyTaskKey::CurrentRead(panel_id),
        None,
        Some(old_slot.clone()),
    );
    let current_ticket = runtime.begin(
        ProxyTaskKey::CurrentRead(panel_id),
        None,
        Some(CancellationSlot::default()),
    );

    assert!(old_slot.is_cancelled());
    assert!(!runtime.is_current(&old_ticket));
    assert!(runtime.is_current(&current_ticket));
}

/// current 目标变化时取消旧操作，但保留其 ticket 以消费最终回执并清除原面板 busy 状态。
#[test]
fn stale_current_operation_keeps_its_result_owner_after_cancellation() {
    let host_id = HostId::parse("host-a").expect("valid test host");
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let old_scope = ProxyCurrentScope {
        selected_host_id: Some(host_id.clone()),
        host_id: host_id.clone(),
        forward_id: Some("forward-old".to_owned()),
        session_id: session_id.clone(),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
    };
    let new_scope = ProxyCurrentScope {
        forward_id: Some("forward-new".to_owned()),
        ..old_scope.clone()
    };
    let slot = CancellationSlot::default();
    let mut runtime = ProxyRuntime::default();
    let panel_id = EntityId::from(9);
    let ticket = runtime.begin(
        ProxyTaskKey::PanelRequest(panel_id, 12),
        Some(old_scope),
        Some(slot.clone()),
    );

    runtime.cancel_stale_current(panel_id, Some(&new_scope));

    assert!(slot.is_cancelled());
    assert!(runtime.is_current(&ticket));
    runtime.finish(&ticket);
    assert!(!runtime.is_current(&ticket));
}

/// 同一 request id 在两个 panel 中互不覆盖，避免 Ports 结果写入完整运维页。
#[test]
fn panel_request_generation_is_scoped_to_entity() {
    let mut runtime = ProxyRuntime::default();
    let primary_panel = EntityId::from(21);
    let ports_panel = EntityId::from(22);
    let primary = runtime.begin(ProxyTaskKey::PanelRequest(primary_panel, 3), None, None);
    let ports = runtime.begin(ProxyTaskKey::PanelRequest(ports_panel, 3), None, None);

    assert!(runtime.is_current(&primary));
    assert!(runtime.is_current(&ports));
    runtime.finish(&ports);
    assert!(runtime.is_current(&primary));
    assert!(!runtime.is_current(&ports));
}

/// 面板切换转发时只取消自身请求，另一个面板的相同 session 操作仍由原 owner 接收回执。
#[test]
fn stale_selection_cancels_only_its_panel_request() {
    let host_id = HostId::parse("host-a").expect("valid test host");
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let scope = ProxyCurrentScope {
        selected_host_id: Some(host_id.clone()),
        host_id: host_id.clone(),
        forward_id: Some("forward-a".to_owned()),
        session_id: session_id.clone(),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
    };
    let changed_scope = ProxyCurrentScope {
        forward_id: Some("forward-b".to_owned()),
        ..scope.clone()
    };
    let primary_id = EntityId::from(31);
    let ports_id = EntityId::from(32);
    let primary_slot = CancellationSlot::default();
    let ports_slot = CancellationSlot::default();
    let mut runtime = ProxyRuntime::default();
    let primary = runtime.begin(
        ProxyTaskKey::PanelRequest(primary_id, 8),
        Some(scope.clone()),
        Some(primary_slot.clone()),
    );
    let ports = runtime.begin(
        ProxyTaskKey::PanelRequest(ports_id, 8),
        Some(scope),
        Some(ports_slot.clone()),
    );

    runtime.cancel_stale_current(primary_id, Some(&changed_scope));

    assert!(primary_slot.is_cancelled());
    assert!(!ports_slot.is_cancelled());
    assert!(runtime.is_current(&primary));
    assert!(runtime.is_current(&ports));
}

/// 恢复与 Unknown 核对只接受回读证明的原 forward 和完整 UI binding。
///
/// @author kongweiguang
#[test]
fn current_proxy_recovery_requires_exact_original_transaction_and_generation() {
    let host_id = HostId::parse("host-recovery").expect("valid test host");
    let binding = UiSessionBinding {
        session_id: SessionId::parse("session-recovery").expect("valid test session"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-recovery").expect("valid test tab"),
            pane_id: PaneId::parse("pane-recovery").expect("valid test pane"),
        },
        session_generation: 42,
    };
    let current = ProxyCurrentSession {
        host_id,
        binding: binding.clone(),
        availability: ProxyCurrentAvailability::Ready,
        application: ProxyCurrentApplication::OutcomeUnknown,
        shell_dialect: RemoteShellDialect::Posix,
    };

    assert!(current_recovery_request_matches(
        Some("original-forward"),
        &current,
        "original-forward",
        &binding,
        ProxyCurrentApplication::OutcomeUnknown,
    ));
    assert!(!current_recovery_request_matches(
        Some("original-forward"),
        &current,
        "another-forward",
        &binding,
        ProxyCurrentApplication::OutcomeUnknown,
    ));

    let wrong_generation = UiSessionBinding {
        session_generation: binding.session_generation + 1,
        ..binding.clone()
    };
    assert!(!current_recovery_request_matches(
        Some("original-forward"),
        &current,
        "original-forward",
        &wrong_generation,
        ProxyCurrentApplication::OutcomeUnknown,
    ));
    assert!(!current_recovery_request_matches(
        Some("original-forward"),
        &current,
        "original-forward",
        &binding,
        ProxyCurrentApplication::Applied,
    ));
}
