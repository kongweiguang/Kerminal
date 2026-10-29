//! 转发目标文案是 HostId 绑定之上的展示投影，不泄露内部 ID 到可见标签。
//!
//! @author kongweiguang

use super::{
    ForwardBindScope, ForwardScenarioCard, bind_scope_mode, forward_copy_address,
    forward_direction_labels, forward_host_endpoint_label, forward_host_id_detail,
    forward_host_label, forward_kind_for_scenario, forward_matches_host, forward_proxy_url,
    forward_scenario_card, forward_session_endpoints, forward_socks_kind,
    forward_socks_proxy_preview,
};
use kerminal_application::operations::ManagedPortForwardSnapshot;
use kerminal_domain::identity::HostId;
use kerminal_domain::operations::{
    ForwardEndpoint, ForwardKind, ForwardState, PortForward, PortForwardConfig, ProxyApplyScope,
    ProxyProtocol,
};

/// 规范化已有目标标签的 SSH 前缀，避免方向摘要再重复协议标识。
///
/// @author kongweiguang
#[test]
fn forward_host_label_strips_duplicate_ssh_prefix() {
    let host_id = HostId::new();

    assert_eq!(
        forward_host_label(&host_id, Some("  SSH · r41 (隔离)  ")),
        "r41 (隔离)"
    );
    assert_eq!(
        forward_host_label(&host_id, Some("Build host")),
        "Build host"
    );
}

/// 隧道页和弹框显示已配置的 SSH 端点，不把 HostNavigation 的协议标记重复展示。
///
/// @author kongweiguang
#[test]
fn forward_host_endpoint_label_strips_only_the_ssh_suffix() {
    assert_eq!(
        forward_host_endpoint_label("alice@build.internal:2222 · SSH"),
        "alice@build.internal:2222"
    );
    assert_eq!(
        forward_host_endpoint_label("build.internal:22"),
        "build.internal:22"
    );
}

/// 无有效保存名称时使用中性标签；HostId 只在专用身份详情中保留。
///
/// @author kongweiguang
#[test]
fn forward_host_label_hides_id_but_keeps_identity_detail() {
    let host_id = HostId::new();
    let id = host_id.as_str();
    let fallback = forward_host_label(&host_id, Some(id));

    assert_ne!(fallback, id);
    assert!(fallback.to_ascii_lowercase().contains("ssh"));
    assert_eq!(forward_host_label(&host_id, Some("  ")), fallback);
    assert!(forward_host_id_detail(&host_id).contains(id));
}

/// 三张操作卡仅是四种既有领域类型的展示分组，普通 SSH 方向保持一一映射。
///
/// @author kongweiguang
#[test]
fn forward_scenario_cards_group_existing_tunnel_kinds() {
    assert_eq!(
        forward_scenario_card(ForwardKind::Local),
        ForwardScenarioCard::HostService
    );
    assert_eq!(
        forward_scenario_card(ForwardKind::Remote),
        ForwardScenarioCard::LocalService
    );
    assert_eq!(
        forward_scenario_card(ForwardKind::Dynamic),
        ForwardScenarioCard::Socks
    );
    assert_eq!(
        forward_scenario_card(ForwardKind::RemoteDynamic),
        ForwardScenarioCard::Socks
    );
}

/// SOCKS 卡进入时默认本机 -D，用户已选远端 -R 后再次打开卡片则保留方向。
///
/// @author kongweiguang
#[test]
fn forward_scenario_selection_preserves_socks_direction() {
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::HostService, ForwardKind::Remote),
        ForwardKind::Local
    );
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::LocalService, ForwardKind::Local),
        ForwardKind::Remote
    );
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::Socks, ForwardKind::Remote),
        ForwardKind::Dynamic
    );
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::Socks, ForwardKind::RemoteDynamic),
        ForwardKind::RemoteDynamic
    );
    assert_eq!(forward_socks_kind(false), ForwardKind::Dynamic);
    assert_eq!(forward_socks_kind(true), ForwardKind::RemoteDynamic);
}

/// 路由摘要将流向与 OpenSSH 参数拆成两行，并保持四种隧道类型各自的方向语义。
///
/// @author kongweiguang
#[test]
fn forward_direction_labels_match_each_tunnel_kind() {
    for (kind, flow_zh, flow_en, openssh) in [
        (ForwardKind::Local, "本机 -> 主机", "Local -> host", "-L"),
        (ForwardKind::Remote, "主机 -> 本机", "Host -> local", "-R"),
        (
            ForwardKind::Dynamic,
            "本机 -> 主机网络",
            "Local -> host network",
            "-D",
        ),
        (
            ForwardKind::RemoteDynamic,
            "主机 -> 本机网络",
            "Host -> local network",
            "remote -R SOCKS",
        ),
    ] {
        let (flow, option) = forward_direction_labels(kind);
        assert!(flow.as_ref() == flow_zh || flow.as_ref() == flow_en);
        assert_eq!(option.as_ref(), openssh);
    }
}

/// 远端 SOCKS 预览匹配 main 的监听地址归一化、IPv6 表示和 POSIX 参数转义。
///
/// @author kongweiguang
#[test]
fn remote_socks_preview_uses_main_host_and_shell_projection() {
    assert_eq!(
        forward_socks_proxy_preview("0.0.0.0", "1080"),
        "export ALL_PROXY='socks5h://127.0.0.1:1080'"
    );
    assert_eq!(
        forward_socks_proxy_preview("", "not-a-port"),
        "export ALL_PROXY='socks5h://127.0.0.1:0'"
    );
    assert_eq!(
        forward_socks_proxy_preview("::", "1080"),
        "export ALL_PROXY='socks5h://[::1]:1080'"
    );
    assert_eq!(
        forward_socks_proxy_preview("2001:db8::1", "1080"),
        "export ALL_PROXY='socks5h://[2001:db8::1]:1080'"
    );
    assert_eq!(
        forward_socks_proxy_preview("host'name", "22"),
        "export ALL_PROXY='socks5h://host'\\''name:22'"
    );
}

/// 监听范围按 main 的三选项归一化，并将固定范围映射到规范监听地址。
///
/// @author kongweiguang
#[test]
fn bind_scope_modes_follow_main_options_and_resolve_addresses() {
    assert_eq!(bind_scope_mode("127.0.0.1"), ForwardBindScope::Loopback);
    assert_eq!(bind_scope_mode("localhost"), ForwardBindScope::Loopback);
    assert_eq!(bind_scope_mode("::1"), ForwardBindScope::Loopback);
    assert_eq!(bind_scope_mode("0.0.0.0"), ForwardBindScope::AllInterfaces);
    assert_eq!(bind_scope_mode("::"), ForwardBindScope::AllInterfaces);
    assert_eq!(bind_scope_mode("10.0.0.8"), ForwardBindScope::Custom);

    assert_eq!(
        ForwardBindScope::Loopback.resolve_host("10.0.0.8"),
        "127.0.0.1"
    );
    assert_eq!(
        ForwardBindScope::AllInterfaces.resolve_host("127.0.0.1"),
        "0.0.0.0"
    );
    assert_eq!(
        ForwardBindScope::Custom.resolve_host("10.0.0.8"),
        "10.0.0.8"
    );
    assert_eq!(ForwardBindScope::Custom.resolve_host(" "), "127.0.0.1");
}

/// 构造与运行时字段无关的 managed snapshot，专门核对四种 SSH 方向的可见端点映射。
///
/// @author kongweiguang
fn managed_forward(
    kind: ForwardKind,
    bind_host: &str,
    bind_port: u16,
    destination: Option<(&str, u16)>,
    actual_bind: Option<(&str, u16)>,
) -> ManagedPortForwardSnapshot {
    let dynamic = matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
    let config = PortForwardConfig {
        host_id: HostId::new(),
        name: "Test tunnel".to_owned(),
        kind,
        bind: ForwardEndpoint::new(bind_host, bind_port).expect("valid bind endpoint"),
        destination: destination.map(|(host, port)| {
            ForwardEndpoint::new(host, port).expect("valid destination endpoint")
        }),
        proxy_protocol: dynamic.then_some(ProxyProtocol::Socks5),
        remote_access_scope: None,
        proxy_apply_scope: ProxyApplyScope::None,
    };
    let mut forward = PortForward::starting("forward-id", config);
    forward.state = if actual_bind.is_some() {
        ForwardState::Active
    } else {
        ForwardState::Stopped
    };
    ManagedPortForwardSnapshot {
        forward,
        persistent: true,
        actual_bind: actual_bind.map(|(host, port)| {
            ForwardEndpoint::new(host, port).expect("valid actual bind endpoint")
        }),
        proxy_url: None,
        proxy_application: None,
        failure_code: None,
    }
}

/// 全局转发清单可混有其它 SSH 主机；切换或失去目标时不能把它们投影到当前右栏。
///
/// @author kongweiguang
#[test]
fn right_rail_forward_projection_is_scoped_to_the_selected_host() {
    let forward = managed_forward(
        ForwardKind::Local,
        "127.0.0.1",
        18080,
        Some(("127.0.0.1", 80)),
        None,
    );
    assert!(forward_matches_host(
        &forward,
        Some(&forward.forward.config.host_id)
    ));
    assert!(!forward_matches_host(&forward, Some(&HostId::new())));
    assert!(!forward_matches_host(&forward, None));
}

/// 普通转发复制地址按 main 的访问侧选择，详情则保留两侧端点顺序。
///
/// @author kongweiguang
#[test]
fn forward_endpoints_and_copy_address_follow_main_direction_rules() {
    let local = managed_forward(
        ForwardKind::Local,
        "127.0.0.1",
        18080,
        Some(("build.internal", 8080)),
        Some(("127.0.0.1", 18080)),
    );
    assert_eq!(
        forward_session_endpoints(&local),
        (
            "build.internal:8080".to_owned(),
            "127.0.0.1:18080".to_owned()
        )
    );
    assert_eq!(forward_copy_address(&local), "127.0.0.1:18080");

    let remote = managed_forward(
        ForwardKind::Remote,
        "0.0.0.0",
        28080,
        Some(("127.0.0.1", 8080)),
        None,
    );
    assert_eq!(
        forward_session_endpoints(&remote),
        ("0.0.0.0:28080".to_owned(), "127.0.0.1:8080".to_owned())
    );
    assert_eq!(forward_copy_address(&remote), "0.0.0.0:28080");

    let dynamic = managed_forward(
        ForwardKind::Dynamic,
        "127.0.0.1",
        1080,
        None,
        Some(("127.0.0.1", 1080)),
    );
    assert_eq!(forward_copy_address(&dynamic), "127.0.0.1:1080");
}

/// 远端 SOCKS URL 只对 RemoteDynamic 暴露；并行测试共享全局 locale，因此出口标签接受两种产品语言。
///
/// @author kongweiguang
#[test]
fn remote_socks_proxy_url_uses_runtime_value_or_safe_stopped_bind_fallback() {
    let stopped = managed_forward(ForwardKind::RemoteDynamic, "0.0.0.0", 1080, None, None);
    assert_eq!(
        forward_proxy_url(&stopped).as_deref(),
        Some("socks5h://127.0.0.1:1080")
    );
    assert_eq!(forward_copy_address(&stopped), "socks5h://127.0.0.1:1080");

    let active = managed_forward(
        ForwardKind::RemoteDynamic,
        "127.0.0.1",
        1080,
        None,
        Some(("127.0.0.1", 1080)),
    );
    let mut active_with_proxy = active;
    active_with_proxy.proxy_url = Some("socks5h://127.0.0.1:1080".to_owned());
    assert_eq!(
        forward_proxy_url(&active_with_proxy).as_deref(),
        Some("socks5h://127.0.0.1:1080")
    );
    let local_egress = forward_session_endpoints(&active_with_proxy).1;
    assert!(matches!(
        local_egress.as_str(),
        "Kerminal 本机网络出口" | "Kerminal local network egress"
    ));
}
