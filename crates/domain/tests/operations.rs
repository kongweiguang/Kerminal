// @author kongweiguang

use kerminal_domain::{
    identity::HostId,
    operations::tmux::TmuxTargetError,
    operations::{
        ForwardEndpoint, ForwardKind, ForwardState, PortForward, PortForwardConfig,
        PortForwardTransitionError, ProxyApplyScope, ProxyProtocol, RemoteAccessScope,
        ServerInfoProbeStatus, ServerInfoSampleTimes, ServerInfoSnapshot, ServerInfoTarget,
        TmuxTarget,
    },
};

/// 只允许本地 tmux 目标引用 profile，避免把本机 cwd/env 误注入远端 SSH。
#[test]
fn tmux_profile_is_scoped_to_a_local_target() {
    let local = TmuxTarget {
        profile_id: Some("dev-shell".to_owned()),
        ..TmuxTarget::default()
    };
    assert!(local.validate().is_ok());

    let remote = TmuxTarget {
        host_id: Some(HostId::new()),
        profile_id: Some("dev-shell".to_owned()),
        ..TmuxTarget::default()
    };
    assert_eq!(
        remote.validate(),
        Err(TmuxTargetError::ProfileRequiresLocalTarget)
    );
}

/// 空白 profile id 不得降级为默认 profile 或被静默忽略。
#[test]
fn tmux_profile_rejects_empty_identifier() {
    let target = TmuxTarget {
        profile_id: Some("  ".to_owned()),
        ..TmuxTarget::default()
    };
    assert_eq!(target.validate(), Err(TmuxTargetError::InvalidProfileId));
}

/// 验证活动期连接断开能进入 Failed，随后仍可通过 Closing 完整释放转发资源。
#[test]
fn active_forward_failure_can_be_closed() {
    let config = PortForwardConfig {
        host_id: HostId::new(),
        name: "local api".to_owned(),
        kind: ForwardKind::Local,
        bind: ForwardEndpoint::new("127.0.0.1", 8080).unwrap(),
        destination: Some(ForwardEndpoint::new("127.0.0.1", 80).unwrap()),
        proxy_protocol: None,
        remote_access_scope: None,
        proxy_apply_scope: Default::default(),
    };
    let mut forward = PortForward::starting("forward-1", config);

    forward.activate().unwrap();
    forward.fail().unwrap();
    assert_eq!(forward.state, ForwardState::Failed);
    forward.begin_close().unwrap();
    forward.stop().unwrap();
    assert_eq!(forward.state, ForwardState::Stopped);
}

/// 验证终态不会被重复标成运行故障，防止 close 完成后的异步通知回滚状态。
#[test]
fn stopped_forward_rejects_late_failure() {
    let config = PortForwardConfig {
        host_id: HostId::new(),
        name: "dynamic".to_owned(),
        kind: ForwardKind::Dynamic,
        bind: ForwardEndpoint::new("127.0.0.1", 1080).unwrap(),
        destination: None,
        proxy_protocol: Some(kerminal_domain::operations::ProxyProtocol::Socks5),
        remote_access_scope: None,
        proxy_apply_scope: Default::default(),
    };
    let mut forward = PortForward::starting("forward-2", config);
    forward.activate().unwrap();
    forward.begin_close().unwrap();
    forward.stop().unwrap();

    assert_eq!(
        forward.fail(),
        Err(PortForwardTransitionError::InvalidTransition {
            from: ForwardState::Stopped,
            to: ForwardState::Failed,
        })
    );
}

/// 旧快照缺少 uptime 字段时仍可读取；未知值不扩张 MCP JSON，实测值使用 camelCase 输出。
///
/// @author kongweiguang
#[test]
fn server_info_uptime_is_optional_and_serde_compatible() {
    let known = serde_json::to_value(server_info_snapshot(Some(3_661))).unwrap();
    assert_eq!(known.get("uptimeSeconds"), Some(&serde_json::json!(3_661)));

    let mut legacy = known;
    legacy
        .as_object_mut()
        .expect("snapshot serializes as an object")
        .remove("uptimeSeconds");
    let restored: ServerInfoSnapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.uptime_seconds, None);

    let unknown = serde_json::to_value(server_info_snapshot(None)).unwrap();
    assert!(
        !unknown
            .as_object()
            .expect("snapshot serializes as an object")
            .contains_key("uptimeSeconds")
    );
}

/// Cached 只有采到真实值才进入 JSON；缺字段的旧快照仍可读且未知值不扩张 MCP 输出。
///
/// @author kongweiguang
#[test]
fn server_info_cached_memory_is_optional_and_serde_compatible() {
    let mut known = server_info_snapshot(None);
    known.memory.cached_bytes = Some(16 * 1024 * 1024);
    let encoded = serde_json::to_value(known).unwrap();
    assert_eq!(
        encoded["memory"]["cachedBytes"],
        serde_json::json!(16 * 1024 * 1024)
    );

    let mut legacy = encoded;
    legacy["memory"]
        .as_object_mut()
        .expect("memory serializes as an object")
        .remove("cachedBytes");
    let restored: ServerInfoSnapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.memory.cached_bytes, None);

    let unknown = serde_json::to_value(server_info_snapshot(None)).unwrap();
    assert!(
        !unknown["memory"]
            .as_object()
            .expect("memory serializes as an object")
            .contains_key("cachedBytes")
    );
}

/// 统一构造最小 ServerInfo 快照，让序列化回归仅关注新字段的兼容语义。
///
/// @author kongweiguang
fn server_info_snapshot(uptime_seconds: Option<u64>) -> ServerInfoSnapshot {
    ServerInfoSnapshot {
        target: ServerInfoTarget::Local,
        captured_at_unix_ms: 1,
        sample_times: ServerInfoSampleTimes::default(),
        uptime_seconds,
        hostname: None,
        operating_system: None,
        architecture: None,
        kernel: None,
        cpu: Default::default(),
        memory: Default::default(),
        disks: Vec::new(),
        network_counters: Vec::new(),
        network_rates: Vec::new(),
        process_count: None,
        running_process_count: None,
        processes: Vec::new(),
        disk_probe_status: ServerInfoProbeStatus::NotSampled,
        network_probe_status: ServerInfoProbeStatus::NotSampled,
        process_probe_status: ServerInfoProbeStatus::NotSampled,
        gpu_probe_status: ServerInfoProbeStatus::NotSampled,
        gpus: Vec::new(),
        npu_probe_status: ServerInfoProbeStatus::NotSampled,
        npus: Vec::new(),
    }
}

/// 验证结构化端点输出对 IPv6 使用括号，避免地址和端口边界含混。
#[test]
fn forward_endpoint_formats_ipv6_without_ambiguity() {
    let endpoint = ForwardEndpoint::new("2001:db8::1", 443).unwrap();
    assert_eq!(endpoint.to_string(), "[2001:db8::1]:443");
}

/// 旧配置缺少新增 scope 字段时仍可读取，新请求则保存明确的代理意图元数据。
#[test]
fn port_forward_scope_metadata_is_backward_compatible() {
    let config = PortForwardConfig {
        host_id: HostId::new(),
        name: "scope roundtrip".to_owned(),
        kind: ForwardKind::RemoteDynamic,
        bind: ForwardEndpoint::new("127.0.0.1", 1080).unwrap(),
        destination: None,
        proxy_protocol: Some(ProxyProtocol::Socks5),
        remote_access_scope: Some(RemoteAccessScope::PrivateNetwork),
        proxy_apply_scope: ProxyApplyScope::ToolOnly,
    };
    let mut document = serde_json::to_value(&config).expect("serialize forward config");
    let fields = document
        .as_object_mut()
        .expect("forward config serializes as an object");
    fields.remove("proxyProtocol");
    fields.remove("remoteAccessScope");
    fields.remove("proxyApplyScope");

    let legacy: PortForwardConfig =
        serde_json::from_value(document).expect("legacy config receives defaults");
    assert_eq!(legacy.proxy_protocol, None);
    assert_eq!(legacy.remote_access_scope, None);
    assert_eq!(legacy.proxy_apply_scope, ProxyApplyScope::None);

    let serialized = serde_json::to_value(config).expect("serialize scoped forward config");
    assert_eq!(serialized["proxyProtocol"], "socks5");
    assert_eq!(serialized["remoteAccessScope"], "privateNetwork");
    assert_eq!(serialized["proxyApplyScope"], "toolOnly");
}
