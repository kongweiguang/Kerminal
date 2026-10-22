//! 运维交互表单的安全映射与字段边界测试。
//!
//! @author kongweiguang

use super::{
    ForwardFormTransition, OperationsFormError, container_command, forward_config,
    next_forward_form_open,
};
use kerminal_domain::{
    identity::HostId,
    operations::{ForwardKind, ProxyApplyScope, ProxyProtocol, RemoteAccessScope},
};

/// 参数行保持单独 argv 项，空格不能被 UI 再解释成 shell 分隔符。
#[test]
fn exec_command_keeps_each_argument_as_one_argv_item() {
    let command = container_command(
        "/usr/bin/tool",
        vec!["--label".to_owned(), "two words".to_owned()],
    )
    .expect("valid structured command");
    assert_eq!(command, ["/usr/bin/tool", "--label", "two words"]);
}

/// 容器程序名不能含空格，避免把 shell command line 伪装成 executable 路径。
#[test]
fn exec_command_rejects_shell_like_program_text() {
    assert_eq!(
        container_command("sh -c echo", Vec::new()),
        Err(OperationsFormError::InvalidProgram),
    );
}

/// 普通 SSH 转发必须有目标端点并且远端监听显式限制在 loopback。
#[test]
fn local_forward_maps_explicit_host_and_endpoints() {
    let host = HostId::parse("host-1").expect("valid host id");
    let config = forward_config(
        Some(host.clone()),
        "database",
        ForwardKind::Local,
        "127.0.0.1",
        "15432",
        "db.internal",
        "5432",
    )
    .expect("valid local forward");
    assert_eq!(config.host_id, host);
    assert_eq!(config.bind.port.get(), 15432);
    assert_eq!(
        config
            .destination
            .as_ref()
            .map(|endpoint| endpoint.port.get()),
        Some(5432)
    );
    assert_eq!(config.remote_access_scope, None);
    assert_eq!(config.proxy_apply_scope, ProxyApplyScope::None);
}

/// Remote 的 bind 位于 SSH 主机，目的端位于发起 SSH 的本机。
#[test]
fn remote_forward_maps_host_listener_to_local_destination() {
    let config = forward_config(
        Some(HostId::parse("host-remote").expect("valid host id")),
        "local-web",
        ForwardKind::Remote,
        "127.0.0.1",
        "18080",
        "127.0.0.1",
        "3000",
    )
    .expect("valid remote forward");

    assert_eq!(config.bind.host, "127.0.0.1");
    assert_eq!(config.bind.port.get(), 18080);
    let destination = config.destination.expect("remote forward has destination");
    assert_eq!(destination.host, "127.0.0.1");
    assert_eq!(destination.port.get(), 3000);
    assert_eq!(
        config.remote_access_scope,
        Some(RemoteAccessScope::Loopback)
    );
}

/// 本地动态转发固定使用 SOCKS5 且不携带静态目标或远端监听范围。
#[test]
fn local_dynamic_forward_has_no_fixed_destination() {
    let config = forward_config(
        Some(HostId::parse("host-socks").expect("valid host id")),
        "local-socks",
        ForwardKind::Dynamic,
        "127.0.0.1",
        "1080",
        "",
        "",
    )
    .expect("valid local dynamic forward");

    assert_eq!(config.destination, None);
    assert_eq!(config.proxy_protocol, Some(ProxyProtocol::Socks5));
    assert_eq!(config.remote_access_scope, None);
}

/// 动态转发使用 SOCKS5 且不接收静态目标，远程动态转发保留 loopback 限制。
#[test]
fn remote_dynamic_forward_has_no_fixed_destination() {
    let config = forward_config(
        Some(HostId::parse("host-2").expect("valid host id")),
        "socks",
        ForwardKind::RemoteDynamic,
        "127.0.0.1",
        "1080",
        "",
        "",
    )
    .expect("valid remote dynamic forward");
    assert_eq!(config.destination, None);
    assert_eq!(config.proxy_protocol, Some(ProxyProtocol::Socks5));
    assert_eq!(
        config.remote_access_scope,
        Some(RemoteAccessScope::Loopback)
    );
}

/// 远程监听地址使用全接口时，领域 metadata 同步反映用户显式的 bind 范围。
#[test]
fn remote_forward_scope_matches_bind_address() {
    let config = forward_config(
        Some(HostId::parse("host-remote").expect("valid host id")),
        "public-socks",
        ForwardKind::RemoteDynamic,
        "0.0.0.0",
        "1080",
        "",
        "",
    )
    .expect("valid remote dynamic forward");
    assert_eq!(
        config.remote_access_scope,
        Some(RemoteAccessScope::AllInterfaces)
    );
}

/// 本机回环别名 localhost 不应被记录成自定义外部监听范围。
#[test]
fn remote_localhost_bind_is_classified_as_loopback() {
    let config = forward_config(
        Some(HostId::parse("host-localhost").expect("valid host id")),
        "localhost-only",
        ForwardKind::Remote,
        "localhost",
        "8080",
        "127.0.0.1",
        "3000",
    )
    .expect("valid remote forward");

    assert_eq!(
        config.remote_access_scope,
        Some(RemoteAccessScope::Loopback)
    );
}

/// IPv6 ULA 属于私有网络范围；普通 hostname 不猜测解析后的真实监听接口。
#[test]
fn remote_scope_handles_ipv6_ula_and_custom_hostname() {
    let host = Some(HostId::parse("host-private").expect("valid host id"));
    let private = forward_config(
        host.clone(),
        "private-v6",
        ForwardKind::Remote,
        "fd12:3456::1",
        "8443",
        "service.internal",
        "443",
    )
    .expect("valid remote forward");
    assert_eq!(
        private.remote_access_scope,
        Some(RemoteAccessScope::PrivateNetwork)
    );

    let custom = forward_config(
        host,
        "custom-host",
        ForwardKind::Remote,
        "gateway.example.test",
        "8443",
        "service.internal",
        "443",
    )
    .expect("valid remote forward");
    assert_eq!(custom.remote_access_scope, Some(RemoteAccessScope::Custom));
}

/// SSH forward 创建不从本机默认目标推断主机，也不接受零端口。
#[test]
fn forward_requires_selected_host_and_nonzero_port() {
    assert_eq!(
        forward_config(
            None,
            "db",
            ForwardKind::Local,
            "127.0.0.1",
            "5432",
            "db",
            "5432"
        ),
        Err(OperationsFormError::MissingSshHost),
    );
    assert_eq!(
        forward_config(
            Some(HostId::parse("host-3").expect("valid host id")),
            "db",
            ForwardKind::Local,
            "127.0.0.1",
            "0",
            "db",
            "5432",
        ),
        Err(OperationsFormError::InvalidEndpoint),
    );
    assert_eq!(
        forward_config(
            Some(HostId::parse("host-4").expect("valid host id")),
            "db",
            ForwardKind::Local,
            "127.0.0.1",
            "5432",
            "db.internal",
            "invalid",
        ),
        Err(OperationsFormError::InvalidEndpoint),
    );
    assert_eq!(
        forward_config(
            Some(HostId::parse("host-5").expect("valid host id")),
            "db",
            ForwardKind::Local,
            "127.0.0.1",
            "5432",
            "",
            "",
        ),
        Err(OperationsFormError::MissingDestination),
    );
}

/// 新建切入表单，失败留在表单中，取消或成功回到列表状态。
///
/// @author kongweiguang
#[test]
fn forward_form_transitions_keep_failure_editable_and_return_after_success() {
    let mut form_open = false;
    form_open = next_forward_form_open(form_open, ForwardFormTransition::Add);
    assert!(form_open);

    form_open = next_forward_form_open(form_open, ForwardFormTransition::CreateFailed);
    assert!(form_open);

    form_open = next_forward_form_open(form_open, ForwardFormTransition::Cancel);
    assert!(!form_open);

    form_open = next_forward_form_open(form_open, ForwardFormTransition::Add);
    assert!(form_open);
    form_open = next_forward_form_open(form_open, ForwardFormTransition::CreateSucceeded);
    assert!(!form_open);
}
