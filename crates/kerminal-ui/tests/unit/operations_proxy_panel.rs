//! SOCKS 代理面板的选择、绑定与 in-flight 防重试约束。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{AgentSessionId, HostId, PaneId, SessionId, TabId},
    operations::{
        ForwardEndpoint, PortForward, PortForwardConfig, ProxyApplicationOutcome, ProxyApplyScope,
        ProxyProtocol,
    },
    sessions::SessionOwner,
};

/// 用短稳定 ID 构造隔离测试主机，避免测试依赖真实配置。
fn host(value: &str) -> HostId {
    HostId::parse(value).expect("test host id is valid")
}

/// 构造最小端口转发快照，显式控制协议、状态和 proxy endpoint。
fn forward_snapshot(
    id: &str,
    host_id: HostId,
    kind: ForwardKind,
    state: ForwardState,
    proxy_url: Option<&str>,
) -> PortForwardSnapshot {
    let actual_bind = ForwardEndpoint::new("127.0.0.1", 1080).expect("valid endpoint");
    PortForwardSnapshot {
        forward: PortForward {
            id: id.to_owned(),
            config: PortForwardConfig {
                host_id,
                name: format!("forward-{id}"),
                kind,
                bind: actual_bind.clone(),
                destination: None,
                proxy_protocol: Some(ProxyProtocol::Socks5),
                remote_access_scope: None,
                proxy_apply_scope: ProxyApplyScope::None,
            },
            state,
        },
        actual_bind,
        proxy_url: proxy_url.map(str::to_owned),
        proxy_application: ProxyApplicationOutcome::not_applied(
            ProxyApplyScope::None,
            proxy_url.is_some(),
        ),
        failure_code: None,
    }
}

/// 构造一个用户拥有的 UI 会话，固定 generation 便于验证迟到回执过滤。
fn ui_session(host_id: HostId, dialect: RemoteShellDialect) -> ProxyCurrentSession {
    ProxyCurrentSession {
        host_id,
        binding: UiSessionBinding {
            session_id: SessionId::new(),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            session_generation: 4,
        },
        availability: ProxyCurrentAvailability::Ready,
        application: ProxyCurrentApplication::NotApplied,
        shell_dialect: dialect,
    }
}

/// 将测试数据组装成单主机投影，Future 状态默认保持不可修改。
fn projection(host_id: HostId, forwards: Vec<PortForwardSnapshot>) -> ProxyPanelProjection {
    ProxyPanelProjection {
        host_id: Some(host_id),
        forwards,
        current_session: None,
        future: ProxyFutureProjection::Unavailable { host_id: None },
    }
}

/// 仅同主机、Active 的动态 SOCKS forward 可被选为应用目标。
#[test]
fn only_active_same_host_dynamic_socks_forward_is_selectable() {
    let current_host = host("host-a");
    let forwards = vec![
        forward_snapshot(
            "active-dynamic",
            current_host.clone(),
            ForwardKind::Dynamic,
            ForwardState::Active,
            Some("socks5h://127.0.0.1:1080"),
        ),
        forward_snapshot(
            "stopped-dynamic",
            current_host.clone(),
            ForwardKind::Dynamic,
            ForwardState::Stopped,
            Some("socks5h://127.0.0.1:1081"),
        ),
        forward_snapshot(
            "regular-forward",
            current_host.clone(),
            ForwardKind::Local,
            ForwardState::Active,
            Some("socks5h://127.0.0.1:1082"),
        ),
        forward_snapshot(
            "other-host",
            host("host-b"),
            ForwardKind::Dynamic,
            ForwardState::Active,
            Some("socks5h://127.0.0.1:1083"),
        ),
        forward_snapshot(
            "missing-endpoint",
            current_host.clone(),
            ForwardKind::RemoteDynamic,
            ForwardState::Active,
            None,
        ),
    ];
    let view = projection(current_host, forwards);

    let eligible = eligible_socks_forwards(&view);
    assert_eq!(eligible.len(), 1);
    assert_eq!(eligible[0].forward.id, "active-dynamic");
}

/// Current apply 必须携带同主机的 UI owner binding，不能使用 Agent/MCP session。
#[test]
fn current_apply_requires_same_host_ui_binding_and_ready_session() {
    let current_host = host("host-a");
    let forward = forward_snapshot(
        "active-dynamic",
        current_host.clone(),
        ForwardKind::Dynamic,
        ForwardState::Active,
        Some("socks5h://127.0.0.1:1080"),
    );
    let mut view = projection(current_host.clone(), vec![forward]);
    let session = ui_session(current_host.clone(), RemoteShellDialect::Posix);
    let expected_session = session.binding.session_id.clone();
    view.current_session = Some(session);
    let panel = ProxyApplicationPanel::new(view);

    assert_eq!(
        panel
            .current_binding()
            .expect("same-host UI session is eligible")
            .session_id,
        expected_session
    );
}

/// 同步快照中的主机、owner 或 readiness 不匹配时拒绝生成 Apply binding。
#[test]
fn current_apply_rejects_other_host_and_non_ui_owner() {
    let current_host = host("host-a");
    let forward = forward_snapshot(
        "active-dynamic",
        current_host.clone(),
        ForwardKind::Dynamic,
        ForwardState::Active,
        Some("socks5h://127.0.0.1:1080"),
    );
    let mut different_host = projection(current_host.clone(), vec![forward.clone()]);
    different_host.current_session = Some(ui_session(host("host-b"), RemoteShellDialect::Posix));
    assert_eq!(
        ProxyApplicationPanel::new(different_host).current_binding(),
        Err(ProxyUnavailableReason::UnsupportedTarget)
    );

    let mut non_ui = projection(current_host.clone(), vec![forward]);
    let mut session = ui_session(current_host, RemoteShellDialect::Posix);
    session.binding.owner = SessionOwner::Agent {
        agent_session_id: AgentSessionId::new(),
    };
    non_ui.current_session = Some(session);
    assert_eq!(
        ProxyApplicationPanel::new(non_ui).current_binding(),
        Err(ProxyUnavailableReason::UnsupportedTarget)
    );
}

/// Unknown shell dialect 不被默认为 POSIX，审阅脚本需等 Root 提供可信方言。
#[test]
fn unknown_shell_dialect_does_not_enable_profile_script_generation() {
    let current_host = host("host-a");
    let forward = forward_snapshot(
        "active-dynamic",
        current_host.clone(),
        ForwardKind::Dynamic,
        ForwardState::Active,
        Some("socks5h://127.0.0.1:1080"),
    );
    let mut view = projection(current_host.clone(), vec![forward]);
    view.current_session = Some(ui_session(current_host, RemoteShellDialect::Unknown));

    let panel = ProxyApplicationPanel::new(view);
    assert!(!panel.profile_script_supported());
}

/// 未知 current apply 结果锁住自动重发入口，必须由 Root 提供刷新后的显式回读。
#[test]
fn unknown_current_outcome_blocks_another_apply() {
    let current_host = host("host-a");
    let forward = forward_snapshot(
        "active-dynamic",
        current_host.clone(),
        ForwardKind::Dynamic,
        ForwardState::Active,
        Some("socks5h://127.0.0.1:1080"),
    );
    let mut view = projection(current_host.clone(), vec![forward]);
    let mut session = ui_session(current_host, RemoteShellDialect::Posix);
    session.application = ProxyCurrentApplication::OutcomeUnknown;
    view.current_session = Some(session);

    assert_eq!(
        ProxyApplicationPanel::new(view).current_binding(),
        Err(ProxyUnavailableReason::OutcomeUnknown)
    );
}

/// 请求逐字段保留同一目标；面板只允许一个在途操作，重复提交不能产生第二次代理注入。
///
/// @author kongweiguang
#[test]
fn only_one_proxy_operation_can_be_pending() {
    let mut panel = ProxyApplicationPanel::new(ProxyPanelProjection::default());
    let first = panel.begin_operation(ProxyOperationRequest {
        kind: ProxyOperationKind::ApplyCurrent,
        host_id: Some(host("host-a")),
        forward_name: "forward-a".to_owned(),
        forward_id: Some("forward-a".to_owned()),
        binding: None,
        expected_revision: None,
        desired_future_forward: None,
        cancellable: true,
    });
    assert_eq!(first, Some(1));
    assert_eq!(
        panel.begin_operation(ProxyOperationRequest {
            kind: ProxyOperationKind::ApplyCurrent,
            host_id: Some(host("host-a")),
            forward_name: "forward-a".to_owned(),
            forward_id: Some("forward-a".to_owned()),
            binding: None,
            expected_revision: None,
            desired_future_forward: None,
            cancellable: true,
        }),
        None
    );
}

/// Future policy 的 CAS revision 原样传递；再次选中同一 forward 表示显式关闭。
#[test]
fn future_toggle_clears_same_forward_with_current_revision() {
    let host_id = host("host-a");
    let state = ProxyFutureProjection::Ready {
        host_id: host_id.clone(),
        forward_id: Some("forward-a".to_owned()),
        revision: 7,
    };
    assert_eq!(
        desired_future_target(&host_id, "forward-a", &state),
        Ok((7, None))
    );
}

/// Future policy 切换到其他 forward 时只设置选中 ID，不猜测当前转发运行范围。
#[test]
fn future_toggle_selects_other_forward_without_changing_revision() {
    let host_id = host("host-a");
    let state = ProxyFutureProjection::Ready {
        host_id: host_id.clone(),
        forward_id: Some("forward-a".to_owned()),
        revision: 3,
    };
    assert_eq!(
        desired_future_target(&host_id, "forward-b", &state),
        Ok((3, Some("forward-b".to_owned())))
    );
}

/// 未知策略和其他主机的回读均不允许构造 CAS 写入目标。
#[test]
fn future_toggle_rejects_unknown_or_other_host_state() {
    let host_id = host("host-a");
    assert_eq!(
        desired_future_target(
            &host_id,
            "forward-a",
            &ProxyFutureProjection::OutcomeUnknown {
                host_id: host_id.clone(),
            },
        ),
        Err(ProxyUnavailableReason::OutcomeUnknown)
    );
    assert_eq!(
        desired_future_target(
            &host_id,
            "forward-a",
            &ProxyFutureProjection::Ready {
                host_id: host("host-b"),
                forward_id: None,
                revision: 0,
            },
        ),
        Err(ProxyUnavailableReason::FutureIntegrationUnavailable)
    );
}

/// 迟到回执必须同时匹配 forward、session ID 和 generation 才能被视为目标结果。
#[test]
fn current_result_requires_exact_forward_and_session_generation() {
    let current = ui_session(host("host-a"), RemoteShellDialect::Posix);
    let pending = PendingProxyOperation {
        request_id: 1,
        kind: ProxyOperationKind::ApplyCurrent,
        host_id: Some(host("host-a")),
        forward_name: "forward-a".to_owned(),
        forward_id: Some("forward-a".to_owned()),
        binding: Some(current.binding.clone()),
        expected_revision: None,
        desired_future_forward: None,
        cancellable: true,
    };
    let make_result = |forward_id: &str, session_generation| ProxyCurrentOperationResult {
        forward_id: forward_id.to_owned(),
        session_id: current.binding.session_id.clone(),
        session_generation,
        outcome: ProxyCurrentResultOutcome::Applied,
        history: None,
    };

    assert!(pending_current_result_matches(
        &pending,
        &make_result("forward-a", current.binding.session_generation)
    ));
    assert!(!pending_current_result_matches(
        &pending,
        &make_result("forward-b", current.binding.session_generation)
    ));
    assert!(!pending_current_result_matches(
        &pending,
        &make_result("forward-a", current.binding.session_generation + 1)
    ));
}

/// Restore 回执不能被 Apply 操作误显示为已成功，反向不匹配也视为未知。
#[test]
fn current_result_success_kind_must_match_requested_action() {
    let pending = PendingProxyOperation {
        request_id: 1,
        kind: ProxyOperationKind::RestoreCurrent,
        host_id: Some(host("host-a")),
        forward_name: "forward-a".to_owned(),
        forward_id: Some("forward-a".to_owned()),
        binding: None,
        expected_revision: None,
        desired_future_forward: None,
        cancellable: true,
    };
    assert!(!current_result_kind_matches(
        &pending,
        &ProxyCurrentResultOutcome::Applied
    ));
    assert!(current_result_kind_matches(
        &pending,
        &ProxyCurrentResultOutcome::Restored
    ));
}

/// 通用未知错误同样锁住 Apply/Restore，避免 Root 未能构造结构化回执时出现重试入口。
#[test]
fn unknown_application_error_locks_the_original_target() {
    let host_id = host("host-a");
    let forward = forward_snapshot(
        "forward-a",
        host_id.clone(),
        ForwardKind::Dynamic,
        ForwardState::Active,
        Some("socks5h://127.0.0.1:1080"),
    );
    let mut snapshot = projection(host_id.clone(), vec![forward]);
    let current = ui_session(host_id.clone(), RemoteShellDialect::Posix);
    snapshot.current_session = Some(current.clone());
    let mut panel = ProxyApplicationPanel::new(snapshot);
    let pending = PendingProxyOperation {
        request_id: 1,
        kind: ProxyOperationKind::ApplyCurrent,
        host_id: Some(host_id),
        forward_name: "forward-a".to_owned(),
        forward_id: Some("forward-a".to_owned()),
        binding: Some(current.binding),
        expected_revision: None,
        desired_future_forward: None,
        cancellable: true,
    };

    panel.mark_operation_outcome_unknown(&pending);
    assert_eq!(
        panel
            .projection
            .current_session
            .as_ref()
            .map(|session| session.application),
        Some(ProxyCurrentApplication::OutcomeUnknown)
    );
    assert_eq!(
        panel.current_binding(),
        Err(ProxyUnavailableReason::OutcomeUnknown)
    );
}

/// Future 写入的通用未知结果将对应主机标为待回读，禁止再次提交同一策略。
#[test]
fn unknown_future_error_requires_policy_readback() {
    let host_id = host("host-a");
    let mut panel = ProxyApplicationPanel::new(projection(host_id.clone(), Vec::new()));
    panel.projection.future = ProxyFutureProjection::Ready {
        host_id: host_id.clone(),
        forward_id: None,
        revision: 4,
    };
    let pending = PendingProxyOperation {
        request_id: 2,
        kind: ProxyOperationKind::SetFuture,
        host_id: Some(host_id.clone()),
        forward_name: "forward-a".to_owned(),
        forward_id: Some("forward-a".to_owned()),
        binding: None,
        expected_revision: Some(4),
        desired_future_forward: Some(Some("forward-a".to_owned())),
        cancellable: false,
    };

    panel.mark_operation_outcome_unknown(&pending);
    assert_eq!(
        desired_future_target(&host_id, "forward-a", &panel.projection.future),
        Err(ProxyUnavailableReason::OutcomeUnknown)
    );
}

/// Future 结果必须对应预期 host、CAS 的恰好一次递增和请求目标，其他回执不冒充成功。
#[test]
fn future_result_requires_matching_host_revision_and_target() {
    let target_host = host("host-a");
    let pending = PendingProxyOperation {
        request_id: 3,
        kind: ProxyOperationKind::SetFuture,
        host_id: Some(target_host.clone()),
        forward_name: "forward-a".to_owned(),
        forward_id: Some("forward-a".to_owned()),
        binding: None,
        expected_revision: Some(8),
        desired_future_forward: Some(Some("forward-a".to_owned())),
        cancellable: false,
    };

    assert!(future_result_matches(
        &pending,
        &target_host,
        Some("forward-a"),
        9
    ));
    assert!(!future_result_matches(
        &pending,
        &host("host-b"),
        Some("forward-a"),
        9
    ));
    assert!(!future_result_matches(
        &pending,
        &target_host,
        Some("forward-b"),
        9
    ));
    assert!(!future_result_matches(
        &pending,
        &target_host,
        Some("forward-a"),
        10
    ));
}
