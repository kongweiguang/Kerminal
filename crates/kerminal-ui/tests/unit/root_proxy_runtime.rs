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

/// 两个面板即使选择不同转发，也必须共享原终端的一次权威回读及回执。
///
/// @author kongweiguang
#[test]
fn two_panels_share_one_terminal_probe_and_receive_the_same_receipt() {
    let host_id = HostId::parse("host-shared").unwrap();
    let scope = ProxyCurrentScope {
        selected_host_id: None,
        host_id: host_id.clone(),
        forward_id: None,
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
    };
    let ports_scope = ProxyCurrentScope {
        selected_host_id: Some(host_id),
        forward_id: Some("forward-a".into()),
        ..scope.clone()
    };
    let mut runtime = ProxyRuntime::default();
    let main = EntityId::from(71);
    let ports = EntityId::from(72);
    let (ticket, _) = runtime
        .begin_or_join_current_read(main, scope.clone())
        .unwrap();
    assert!(
        runtime
            .begin_or_join_current_read(ports, ports_scope.clone())
            .is_none()
    );
    assert_eq!(runtime.active.len(), 1);
    let completed = runtime.finish_current_read(&ticket).unwrap();
    assert_eq!(completed.waiters.get(&main), Some(&scope));
    assert_eq!(completed.waiters.get(&ports), Some(&ports_scope));
    assert!(runtime.active.is_empty());
}

/// 选择为空的主面板不能在 Ports 面板执行或回读期间另起同一终端的探针。
///
/// @author kongweiguang
#[test]
fn terminal_operation_blocks_other_panel_even_when_selection_differs() {
    let host_id = HostId::parse("host-shared").unwrap();
    let scope = ProxyCurrentScope {
        selected_host_id: Some(host_id.clone()),
        host_id,
        forward_id: Some("forward-a".into()),
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
    };
    let main_scope = ProxyCurrentScope {
        selected_host_id: None,
        forward_id: None,
        ..scope.clone()
    };
    let mut runtime = ProxyRuntime::default();
    let ticket = runtime.begin(
        ProxyTaskKey::PanelRequest(EntityId::from(73), 1),
        Some(scope),
        Some(CancellationSlot::default()),
    );
    assert!(runtime.has_current_operation(&main_scope));
    runtime.finish(&ticket);
    assert!(!runtime.has_current_operation(&main_scope));
}

/// 只有同一 UI session 的运行态观察可保留旧快照；断开、切换和未知均立即清除。
///
/// @author kongweiguang
#[test]
fn refresh_retention_requires_running_observed_binding() {
    let host_id = HostId::parse("host-a").unwrap();
    let scope = ProxyCurrentScope {
        selected_host_id: Some(host_id.clone()),
        host_id,
        forward_id: Some("forward-a".into()),
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
    };
    let binding = UiSessionBinding {
        session_id: scope.session_id.clone(),
        owner: scope.owner.clone(),
        session_generation: 5,
    };
    assert_eq!(
        trusted_proxy_refresh_binding(&scope, Some((binding.clone(), SessionLifecycle::Running))),
        Some(binding.clone())
    );
    assert!(
        trusted_proxy_refresh_binding(&scope, Some((binding.clone(), SessionLifecycle::Exited)))
            .is_none()
    );
    assert!(trusted_proxy_refresh_binding(&scope, None).is_none());
    let different = UiSessionBinding {
        session_id: SessionId::new(),
        ..binding
    };
    assert!(
        trusted_proxy_refresh_binding(&scope, Some((different, SessionLifecycle::Running)))
            .is_none()
    );
}

/// 回读刚结束后重连仍可能改变 generation，分发必须再次核验服务观察。
///
/// @author kongweiguang
#[test]
fn refresh_readback_rejects_reconnect_and_new_generation() {
    let binding = UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 5,
    };
    assert!(proxy_readback_matches_observation(&binding, None, false));
    assert!(!proxy_readback_matches_observation(&binding, None, true));
    assert!(!proxy_readback_matches_observation(
        &binding,
        Some((binding.clone(), SessionLifecycle::Exited)),
        false
    ));
    let changed = UiSessionBinding {
        session_generation: 6,
        ..binding.clone()
    };
    assert!(!proxy_readback_matches_observation(
        &binding,
        Some((changed, SessionLifecycle::Running)),
        false
    ));
    assert!(proxy_readback_matches_observation(
        &binding,
        Some((binding.clone(), SessionLifecycle::Running)),
        false
    ));
}

/// 观察者离开后取消的探针必须先收尾，重返同一终端不能与旧回读并发。
///
/// @author kongweiguang
#[test]
fn returning_panel_waits_for_cancelled_probe_to_release_its_reservation() {
    let host_id = HostId::parse("host-shared").unwrap();
    let scope = ProxyCurrentScope {
        selected_host_id: Some(host_id.clone()),
        host_id,
        forward_id: Some("forward-a".into()),
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
    };
    let mut runtime = ProxyRuntime::default();
    let panel = EntityId::from(74);
    let (ticket, cancellation) = runtime
        .begin_or_join_current_read(panel, scope.clone())
        .unwrap();
    runtime.cancel_stale_current(panel, None);
    runtime.cancel_unobserved_current_read();
    assert!(cancellation.is_cancelled());
    assert!(runtime.begin_or_join_current_read(panel, scope).is_none());
    let completed = runtime.finish_current_read(&ticket).unwrap();
    assert!(completed.cancellation.is_cancelled());
    assert_eq!(completed.waiters.len(), 1);
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

/// 同值回读不能掩盖 CAS 失败；只有原主机的目标与版本均符合才不提示错误。
///
/// @author kongweiguang
#[test]
fn future_proxy_feedback_checks_write_and_readback_independently() {
    let host_id = HostId::parse("host-a").unwrap();
    let request = FutureProxyApplyRequest {
        host_id: host_id.clone(),
        expected_revision: 4,
        forward_id: Some("forward-a".into()),
    };
    let state = FutureProxyApplyState {
        host_id,
        revision: 5,
        forward_id: Some("forward-a".into()),
    };
    assert!(
        future_operation_error(&Ok((Ok(state.clone()), Ok(state.clone()))), &request).is_none()
    );
    assert!(
        future_operation_error(
            &Ok((Err(AppError::conflict("policy changed")), Ok(state.clone()))),
            &request
        )
        .is_some()
    );
    let mut changed = state.clone();
    changed.revision = 6;
    assert!(future_operation_error(&Ok((Ok(state.clone()), Ok(changed))), &request).is_some());
    assert!(
        future_operation_error(
            &Ok((Ok(state), Err(AppError::timeout("readback")))),
            &request
        )
        .is_some()
    );
}

/// 只恢复部分变量不能被已清除的代理跟踪状态误报为完全恢复。
///
/// @author kongweiguang
#[test]
fn partial_proxy_restore_is_reported_even_after_readback_clears_tracking() {
    let result = TerminalProxyApplyResult {
        forward_id: "forward-a".into(),
        session_id: SessionId::new(),
        session_generation: 1,
        outcome: TerminalProxyApplyOutcome::PartiallyRestored,
        history_recorded: None,
        history_skip_reason: None,
        history_error_code: None,
    };
    assert!(matches!(
        current_operation_status(
            true,
            &Ok(result),
            ProxyCurrentApplication::NotApplied,
            false
        ),
        ProxyOperationStatus::Failed(_)
    ));
}
