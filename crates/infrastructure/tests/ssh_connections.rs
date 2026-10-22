//! SSH shared-lease、channel isolation 与 fail-closed 行为的 loopback 集成验证。
//!
//! @author kongweiguang

mod support;

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use kerminal_application::{
    AppError, AppResult,
    connections::{
        CommandEvent, CommandStreamHandle, ConnectionIntent, ExecRequest, HostConnectionRepository,
        HostKeyChallenge, HostKeyPeer, HostKeyStatus, HostKeyTrustReceipt, HostKeyTrustRequest,
        KnownHostsRepository, OpenSftpRequest, RemoteConnectionHandle, RemoteConnectionLease,
        RemoteConnectionService, RemoteForwardHandle, RemoteForwardRequest, RemoteShellKind,
        SftpChannelHandle, ShellScriptRequest, TerminalOpenRequest, TunnelChannelHandle,
        TunnelRequest,
    },
    sessions::{SessionTransport, SessionTransportFactory},
};
use kerminal_domain::{
    connections::{
        ChannelLane, ConnectionCapabilities, CredentialRef, HostConnectionProfile, HostEndpoint,
        ProxyProfile, ProxyProtocol, RemoteProtocol, RemoteShellDialect, SshAuthentication,
        SshJumpHost, SshRoute,
        credentials::{CredentialKind, CredentialMaterial, CredentialReference, CredentialScope},
    },
    identity::HostId,
    sessions::{SessionTarget, SshInitialCommand, SshSessionTarget, TerminalSize},
};
#[allow(
    dead_code,
    reason = "测试通过 path import 复用产品模块，但只覆盖其中一部分能力"
)]
#[path = "../src/connections/exec.rs"]
mod exec;
use exec::encode_exec_command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use support::ssh_harness::{
    SshServerFixture, SshServerOptions, TestCredentialResolver, TestKnownHostsRepository,
    connection_service,
};

use kerminal_infrastructure::connections::{
    RusshRemoteConnectionDependencies, RusshRemoteConnectionService, SshSessionTransportFactory,
};

/// 使用 loopback fixture 地址生成不引用用户 home/config 的 SSH profile。
fn profile_for(
    server: &SshServerFixture,
    protocol: RemoteProtocol,
    authentication: SshAuthentication,
) -> HostConnectionProfile {
    HostConnectionProfile {
        host_id: HostId::new(),
        protocol,
        endpoint: HostEndpoint {
            host: server.address.ip().to_string(),
            port: server.address.port(),
            username: "kerminal".to_owned(),
        },
        authentication: Some(authentication),
        rdp_password: None,
        route: SshRoute::default(),
        shell_dialect: RemoteShellDialect::Posix,
        connect_timeout_ms: 2_000,
        keepalive_interval_ms: None,
    }
}

/// 给 loopback server 注入精确指纹，测试普通连接路径必须已显式受信。
fn trusted_known_hosts(
    server: &SshServerFixture,
    endpoint: &HostEndpoint,
) -> Arc<TestKnownHostsRepository> {
    let known_hosts = Arc::new(TestKnownHostsRepository::default());
    known_hosts.trust_fingerprint(endpoint, "ecdsa-sha2-nistp256", &server.fingerprint_sha256);
    known_hosts
}

/// 构造测试用密码认证 profile 与隔离凭据解析器。
fn password_setup(
    server: &SshServerFixture,
) -> (HostConnectionProfile, Arc<TestCredentialResolver>) {
    let credential_ref = CredentialRef::new("test:password").expect("static ref is valid");
    let profile = profile_for(
        server,
        RemoteProtocol::Ssh,
        SshAuthentication::Password {
            secret: credential_ref,
        },
    );
    let credentials = Arc::new(TestCredentialResolver::default());
    credentials.insert_password("test:password", "test-password");
    (profile, credentials)
}

/// 用明确的 host id 构造普通 SSH PTY 目标，不依赖当前 UI 或默认目标。
///
/// @author kongweiguang
fn ssh_session_target(host_id: HostId) -> SessionTarget {
    SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: None,
        command: None,
    })
}

/// 等待 loopback server 观察到指定次数的资源事件，避免靠固定 sleep 猜测清理完成。
///
/// @author kongweiguang
async fn wait_for_counter(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while counter.load(Ordering::Relaxed) < expected {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("SSH fixture reaches the expected lifecycle event");
}

/// 建立仅用于 remote-forward 的已信任 SSH lease，测试不读取外部配置或用户凭据。
///
/// @author kongweiguang
async fn open_tunnel_lease(server: &SshServerFixture) -> RemoteConnectionLease {
    let (profile, credentials) = password_setup(server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(server, &profile.endpoint);
    connection_service(profile, credentials, known_hosts)
        .connect(host_id, ConnectionIntent::Tunnel, CancellationToken::new())
        .await
        .expect("loopback tunnel lease opens with an explicitly trusted host key")
}

/// 等待所有已接受的 TCP session 收尾，防止只观察到第一个已关闭连接就结束用例。
///
/// @author kongweiguang
async fn wait_for_all_fixture_connections_closed(server: &SshServerFixture) {
    let accepted = server.accepted_connections.load(Ordering::Relaxed);
    assert!(accepted > 0, "fixture accepted at least one SSH connection");
    wait_for_counter(&server.closed_connections, accepted).await;
}

/// 固定端口请求必须以请求值作为 effective endpoint，并以相同端口撤销 listener。
///
/// @author kongweiguang
#[tokio::test]
async fn fixed_remote_forward_uses_requested_port_and_cancels_same_port() {
    let server = SshServerFixture::spawn(SshServerOptions {
        accept_remote_forward: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let lease = open_tunnel_lease(&server).await;
    let requested_port = 30_471;
    let mut listener = lease
        .request_remote_forward(
            RemoteForwardRequest {
                bind_host: "127.0.0.1".to_owned(),
                bind_port: requested_port,
            },
            CancellationToken::new(),
        )
        .await
        .expect("fixed remote-forward accepts russh's empty success reply");

    assert_eq!(listener.bind_endpoint(), ("127.0.0.1", requested_port));
    listener
        .close()
        .await
        .expect("remote listener is explicitly cancelled");
    assert_eq!(
        *server
            .remote_forward_cancellations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
        vec![("127.0.0.1".to_owned(), u32::from(requested_port))]
    );
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "fixed remote-forward test complete",
        )
        .await
        .expect("SSH lease closes after its listener");
    server.shutdown().await;
}

/// 动态端口仅接受服务器回传的非零有效端口，并将其用于 listener close。
///
/// @author kongweiguang
#[tokio::test]
async fn dynamic_remote_forward_uses_server_assigned_port() {
    let server = SshServerFixture::spawn(SshServerOptions {
        accept_remote_forward: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let lease = open_tunnel_lease(&server).await;
    let assigned_port = 41_321;
    let mut listener = lease
        .request_remote_forward(
            RemoteForwardRequest {
                bind_host: "127.0.0.1".to_owned(),
                bind_port: 0,
            },
            CancellationToken::new(),
        )
        .await
        .expect("server assigns a dynamic remote-forward port");

    assert_eq!(
        listener.bind_endpoint(),
        ("127.0.0.1", assigned_port as u16)
    );
    listener
        .close()
        .await
        .expect("assigned listener is cancelled");
    assert_eq!(
        *server
            .remote_forward_cancellations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
        vec![("127.0.0.1".to_owned(), assigned_port)]
    );
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "dynamic remote-forward test complete",
        )
        .await
        .expect("SSH lease closes after its listener");
    server.shutdown().await;
}

/// 依赖把拒绝和畸形成功回执合并，固定请求须撤销 ACK 才能重试，动态请求须关闭旧 peer。
/// 保留四次实际服务端请求，并验证同 peer 动态重试不出网，避免把未知 listener 交给新 pending。
///
/// @author kongweiguang
#[tokio::test]
async fn rejected_remote_forward_requires_cancel_ack_or_peer_reconnect() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let lease = open_tunnel_lease(&server).await;
    let fixed = RemoteForwardRequest {
        bind_host: "127.0.0.1".to_owned(),
        bind_port: 30_472,
    };
    let dynamic = RemoteForwardRequest {
        bind_host: "127.0.0.1".to_owned(),
        bind_port: 0,
    };

    let mut failure_codes = Vec::new();
    for request in [fixed.clone(), fixed.clone(), dynamic.clone()] {
        let error = lease
            .request_remote_forward(request, CancellationToken::new())
            .await
            .err()
            .expect("rejected request must not publish a listener");
        failure_codes.push(error.public_code());
    }
    let blocked_error = lease
        .request_remote_forward(dynamic.clone(), CancellationToken::new())
        .await
        .err()
        .expect("ambiguous dynamic rejection must block a new pending slot");
    let requests_before_reconnect = server.remote_forward_request_count.load(Ordering::Relaxed);
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "rejected remote-forward test complete",
        )
        .await
        .expect("unknown setup owner closes before reconnect");
    wait_for_all_fixture_connections_closed(&server).await;

    let reconnected_lease = open_tunnel_lease(&server).await;
    let error = reconnected_lease
        .request_remote_forward(dynamic, CancellationToken::new())
        .await
        .err()
        .expect("a new peer reaches the server instead of inheriting blocked pending");
    failure_codes.push(error.public_code());
    reconnected_lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "rejected remote-forward reconnect test complete",
        )
        .await
        .expect("reconnected setup lease closes");
    wait_for_all_fixture_connections_closed(&server).await;
    let request_count = server.remote_forward_request_count.load(Ordering::Relaxed);
    let cancellations = server
        .remote_forward_cancellations
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    server.shutdown().await;

    assert_eq!(
        failure_codes,
        vec![kerminal_application::PublicErrorCode::Infrastructure; 4]
    );
    assert_eq!(
        blocked_error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(
        blocked_error.public_message(),
        "此前动态转发建立结果未知，请关闭此连接并重新连接后重试"
    );
    assert_eq!(requests_before_reconnect, 3);
    assert_eq!(
        request_count, 4,
        "only reconnect permits another dynamic request"
    );
    assert_eq!(
        cancellations,
        vec![
            ("127.0.0.1".to_owned(), u32::from(fixed.bind_port)),
            ("127.0.0.1".to_owned(), u32::from(fixed.bind_port)),
        ]
    );
}

/// 取消后的动态端口结果未知，同一 peer 不能重建 pending 接收旧 channel；关闭 setup lease 最终回收。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_dynamic_remote_forward_closes_setup_lease() {
    let server = SshServerFixture::spawn(SshServerOptions {
        accept_remote_forward: true,
        remote_forward_first_request_delay: std::time::Duration::from_millis(150),
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let lease = open_tunnel_lease(&server).await;
    let cancel = CancellationToken::new();
    let request_lease = lease.clone();
    let request_cancel = cancel.clone();
    let request_task = tokio::spawn(async move {
        request_lease
            .request_remote_forward(
                RemoteForwardRequest {
                    bind_host: "127.0.0.1".to_owned(),
                    bind_port: 0,
                },
                request_cancel,
            )
            .await
    });
    wait_for_counter(&server.remote_forward_request_count, 1).await;
    cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), request_task)
        .await
        .expect("cancelled request returns promptly")
        .expect("request task does not panic");
    let error = result
        .err()
        .expect("cancelled request is not published as a listener");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    let retry_error = lease
        .request_remote_forward(
            RemoteForwardRequest {
                bind_host: "127.0.0.1".to_owned(),
                bind_port: 0,
            },
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("unknown dynamic result blocks a new pending slot on this peer");
    assert_eq!(
        retry_error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(
        retry_error.public_message(),
        "此前动态转发建立结果未知，请关闭此连接并重新连接后重试"
    );
    assert_eq!(
        server.remote_forward_request_count.load(Ordering::Relaxed),
        1,
        "blocked retry must not create another remote listener"
    );

    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "cancelled dynamic remote-forward setup complete",
        )
        .await
        .expect("failed setup closes its dedicated SSH lease");
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 固定申请取消后只由撤销 ACK 允许重用端口，清理成功仍须返回原始取消而非发布 listener。
/// 请求和取消在同一作用域内驱动，避免测试看门狗丢弃 JoinHandle 后遗留脱离的请求任务。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_fixed_remote_forward_reuses_port_only_after_cancel_ack() {
    let server = SshServerFixture::spawn(SshServerOptions {
        accept_remote_forward: true,
        remote_forward_first_request_delay: std::time::Duration::from_millis(500),
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let lease = open_tunnel_lease(&server).await;
    let request = RemoteForwardRequest {
        bind_host: "127.0.0.1".to_owned(),
        bind_port: 30_473,
    };
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::join!(
        lease.request_remote_forward(request.clone(), cancel.clone()),
        async {
            wait_for_counter(&server.remote_forward_request_count, 1).await;
            cancel.cancel();
        }
    );
    let error = result
        .err()
        .expect("cancelled setup never publishes a listener");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    let mut listener = lease
        .request_remote_forward(request.clone(), CancellationToken::new())
        .await
        .expect("confirmed cancellation releases the fixed port for retry");
    listener.close().await.expect("retried listener cancels");
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "cancelled fixed remote-forward retry complete",
        )
        .await
        .expect("both setup and retry share a fully joined SSH owner");
    wait_for_all_fixture_connections_closed(&server).await;
    let request_count = server.remote_forward_request_count.load(Ordering::Relaxed);
    let cancellations = server
        .remote_forward_cancellations
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    server.shutdown().await;
    assert_eq!(request_count, 2);
    assert_eq!(
        cancellations,
        vec![
            ("127.0.0.1".to_owned(), u32::from(request.bind_port)),
            ("127.0.0.1".to_owned(), u32::from(request.bind_port)),
        ]
    );
}

/// 等待第二个 PTY 的唯一输入标记抵达，证明关闭 sibling lease 后该 channel 仍可用。
///
/// @author kongweiguang
async fn wait_for_terminal_marker(server: &SshServerFixture, marker: &[u8]) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let captured = server
                .terminal_input
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            if captured
                .windows(marker.len())
                .any(|window| window == marker)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("terminal marker reaches the SSH fixture");
}

/// 认证后的随机 nonce 探针区分 Bash、Zsh、Fish 与 PowerShell，不能从 Posix 方言猜 shell kind。
#[tokio::test]
async fn authenticated_probe_distinguishes_supported_shells() {
    let cases = [
        (RemoteShellKind::Bash, RemoteShellDialect::Posix),
        (RemoteShellKind::Zsh, RemoteShellDialect::Posix),
        (RemoteShellKind::Fish, RemoteShellDialect::Fish),
        (RemoteShellKind::PowerShell, RemoteShellDialect::PowerShell),
    ];
    for (expected_kind, expected_dialect) in cases {
        let server = SshServerFixture::spawn(SshServerOptions {
            shell_probe_kind: expected_kind,
            ..SshServerOptions::default()
        })
        .await
        .expect("loopback server starts");
        let (mut profile, credentials) = password_setup(&server);
        profile.shell_dialect = RemoteShellDialect::Unknown;
        let host_id = profile.host_id.clone();
        let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
        let service = connection_service(profile, credentials, known_hosts);
        let lease = service
            .connect(
                host_id,
                ConnectionIntent::Terminal,
                CancellationToken::new(),
            )
            .await
            .expect("trusted SSH connection and bounded probe succeed");

        assert_eq!(lease.shell_kind(), expected_kind);
        assert_eq!(lease.shell_dialect(), expected_dialect);
        lease
            .close_until(
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                "shell probe test complete",
            )
            .await
            .expect("all ssh tasks close within one deadline");
        server.shutdown().await;
    }
}

/// 应用保存器生成的 proxy-UUID scope 必须通过生产 SSH route 校验并到达同一 vault resolver。
#[tokio::test]
async fn generated_proxy_scope_reaches_runtime_credential_resolver() {
    let host_id = HostId::new();
    let scope = format!("proxy-{}", uuid::Uuid::new_v4().simple());
    let reference = CredentialReference::new(
        CredentialKind::parse("proxy-host").expect("proxy kind is valid"),
        host_id.clone(),
        CredentialScope::parse(&scope).expect("generated transaction scope is valid"),
        CredentialMaterial::Password,
    )
    .expect("application-compatible typed proxy ref is valid")
    .reference()
    .clone();
    let credentials = Arc::new(TestCredentialResolver::default());
    credentials.insert_password(reference.as_str(), "test proxy secret");

    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback ephemeral port is available");
    let proxy_port = proxy_listener
        .local_addr()
        .expect("loopback endpoint is readable")
        .port();
    let proxy_accept = tokio::spawn(async move {
        let (stream, _) = proxy_listener
            .accept()
            .await
            .expect("test proxy accepts the resolved connection");
        drop(stream);
    });

    let mut profile = HostConnectionProfile {
        host_id: host_id.clone(),
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "target.example".to_owned(),
            port: 22,
            username: "target-user".to_owned(),
        },
        authentication: Some(SshAuthentication::Agent),
        rdp_password: None,
        route: SshRoute::default(),
        shell_dialect: RemoteShellDialect::Unknown,
        connect_timeout_ms: 1_000,
        keepalive_interval_ms: None,
    };
    profile.route.proxy = Some(ProxyProfile {
        protocol: ProxyProtocol::Socks5,
        endpoint: HostEndpoint {
            host: "127.0.0.1".to_owned(),
            port: proxy_port,
            username: "proxy-user".to_owned(),
        },
        password: Some(reference),
    });
    let service = connection_service(
        profile,
        credentials.clone(),
        Arc::new(TestKnownHostsRepository::default()),
    );

    let error = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .err()
        .expect("test proxy rejects the route after credential resolution");
    proxy_accept
        .await
        .expect("loopback proxy accept task completes");
    assert_eq!(
        credentials.resolve_count.load(Ordering::Relaxed),
        1,
        "the route validator accepts the generated scope and invokes the shared resolver"
    );
    assert!(matches!(error, AppError::Infrastructure { .. }));
}

/// 已过期的关闭期限先取消底层 transport，下一次重试必须直接成功，不能靠忽略一次清理错误再确认。
#[tokio::test]
async fn expired_lease_close_cancels_transport_and_later_call_joins() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("trusted SSH connection succeeds");

    let error = lease
        .close_until(tokio::time::Instant::now(), "expired close test")
        .await
        .expect_err("an already expired deadline is reported");
    assert!(matches!(error, AppError::Timeout { .. }));
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "retry join after expired close",
        )
        .await
        .expect("the first retry joins the retained locally cancelled transport");
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "confirm joined close",
        )
        .await
        .expect("closed state is set only after Handle completion");
    server.shutdown().await;
}

/// 只对 nonce 已确认的 shell 用一次 exec 启动 hook，PTY 不承载初始化脚本；关闭到期仍保留 join owner。
///
/// @author kongweiguang
#[tokio::test]
async fn detected_zsh_fish_and_powershell_receive_their_shell_hook() {
    let cases = [
        (
            RemoteShellKind::Zsh,
            RemoteShellDialect::Posix,
            b"zsh -l -i".as_slice(),
        ),
        (
            RemoteShellKind::Fish,
            RemoteShellDialect::Fish,
            b"fish --login --interactive --init-command".as_slice(),
        ),
        (
            RemoteShellKind::PowerShell,
            RemoteShellDialect::PowerShell,
            b"$__kerminal_args = @('-NoLogo', '-NoExit', '-Command', $__kerminal_child_command)"
                .as_slice(),
        ),
    ];
    for (shell_kind, dialect, expected_startup) in cases {
        let server = SshServerFixture::spawn(SshServerOptions {
            shell_probe_kind: shell_kind,
            ..SshServerOptions::default()
        })
        .await
        .expect("loopback server starts");
        let (mut profile, credentials) = password_setup(&server);
        profile.shell_dialect = RemoteShellDialect::Unknown;
        let host_id = profile.host_id.clone();
        let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
        let service = connection_service(profile, credentials, known_hosts);
        let lease = service
            .connect(
                host_id,
                ConnectionIntent::Terminal,
                CancellationToken::new(),
            )
            .await
            .expect("trusted SSH connection succeeds");
        let terminal = lease
            .open_terminal(
                TerminalOpenRequest {
                    term: "xterm-256color".to_owned(),
                    size: TerminalSize::default(),
                    environment: BTreeMap::new(),
                    command: None,
                    program: None,
                    args: Vec::new(),
                    working_directory: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("PTY and interactive shell exec are acknowledged");
        assert_eq!(terminal.shell_dialect, dialect);
        assert!(terminal.shell_integration_enabled);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                terminal.control.wait_exit(),
            )
            .await
            .is_err()
        );
        let matching_exec_count = server
            .exec_commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .filter(|command| {
                command
                    .windows(expected_startup.len())
                    .any(|window| window == expected_startup)
            })
            .count();
        assert_eq!(
            matching_exec_count, 1,
            "shell startup uses exactly one exec"
        );
        assert!(
            server
                .terminal_input
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_empty(),
            "shell hook initialization must not enter PTY stdin"
        );
        let expired = terminal
            .control
            .close_until(tokio::time::Instant::now())
            .await
            .expect_err("expired PTY deadline cancels the read pump but retains its join owner");
        assert!(matches!(expired, AppError::Timeout { .. }));
        terminal
            .control
            .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
            .await
            .expect("retry sends channel close and joins the retained reader task");
        drop(terminal);
        lease
            .close_until(
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                "shell hook test complete",
            )
            .await
            .expect("parent SSH task joins");
        server.shutdown().await;
    }
}

/// 结构化命令使用固定 payload，测试只验证 transport 事件，不依赖服务端 shell。
fn exec_request() -> ExecRequest {
    ExecRequest {
        program: "fixture-command".to_owned(),
        args: vec!["argument with spaces".to_owned()],
        environment: BTreeMap::new(),
        working_directory: None,
    }
}

/// 通过独立 SFTP channel 验证 ExtendedData 不会混入 SFTP 协议读取字节。
#[tokio::test]
async fn sftp_subsystem_discards_stderr_and_preserves_initial_protocol_bytes() {
    let server = SshServerFixture::spawn(SshServerOptions {
        sftp_setup_stdout: b"sftp-protocol-packet".to_vec(),
        sftp_setup_stderr: b"diagnostic-only".to_vec(),
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Sftp, CancellationToken::new())
        .await
        .expect("trusted SSH connection succeeds");
    let mut channel = lease
        .open_sftp(
            OpenSftpRequest {
                lane: ChannelLane::Bulk,
            },
            CancellationToken::new(),
        )
        .await
        .expect("SFTP subsystem accepted");
    let mut actual = vec![0; b"sftp-protocol-packet".len()];
    channel
        .read_exact(&mut actual)
        .await
        .expect("protocol preface is readable");
    assert_eq!(actual, b"sftp-protocol-packet");
    drop(channel);
    lease.close("test complete").await.expect("lease closes");
    server.shutdown().await;
}

/// 未知主机指纹必须在凭据解析前阻断连接，避免把 secret 发给未验证 peer。
#[tokio::test]
async fn unknown_host_key_fails_before_resolving_credentials() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let service = connection_service(
        profile,
        credentials.clone(),
        Arc::new(TestKnownHostsRepository::default()),
    );
    let error = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .err()
        .expect("unknown server key is rejected");
    assert!(matches!(error, AppError::HostKeyUnknown));
    assert_eq!(
        credentials
            .resolve_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    server.shutdown().await;
}

/// 与已保存指纹不符时必须在密码解析前拒绝连接并保留 changed 分类。
#[tokio::test]
async fn changed_host_key_fails_before_resolving_credentials() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = Arc::new(TestKnownHostsRepository::default());
    known_hosts.trust_fingerprint(
        &profile.endpoint,
        "ecdsa-sha2-nistp256",
        "SHA256:previous-host-key",
    );
    let service = connection_service(profile, credentials.clone(), known_hosts);

    let error = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .err()
        .expect("changed server key is rejected");

    assert!(matches!(error, AppError::HostKeyChanged));
    assert_eq!(
        credentials
            .resolve_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    server.shutdown().await;
}

/// 显式信任会重握手并比对完整指纹；保存 endpoint 快照以适配 service 消费 profile。
///
/// @author kongweiguang
#[tokio::test]
async fn host_key_trust_requires_second_handshake_and_exact_fingerprint() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let endpoint = profile.endpoint.clone();
    let known_hosts = Arc::new(TestKnownHostsRepository::default());
    let service = connection_service(profile, credentials, known_hosts.clone());
    let challenge = service
        .inspect_host_key(
            host_id.clone(),
            HostKeyPeer::Target,
            CancellationToken::new(),
        )
        .await
        .expect("host key inspection succeeds before authentication");
    assert_eq!(challenge.status, HostKeyStatus::Unknown);

    let incorrect = service
        .trust_host_key(
            HostKeyTrustRequest {
                host_id: host_id.clone(),
                peer: HostKeyPeer::Target,
                endpoint: endpoint.clone(),
                expected_fingerprint_sha256: "SHA256:incorrect".to_owned(),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("mismatched confirmation is rejected");
    assert!(matches!(incorrect, AppError::HostKeyTrustMismatch));
    let still_unknown = service
        .inspect_host_key(
            host_id.clone(),
            HostKeyPeer::Target,
            CancellationToken::new(),
        )
        .await
        .expect("mismatched trust attempt leaves current state untouched");
    assert_eq!(still_unknown.status, HostKeyStatus::Unknown);

    let receipt = service
        .trust_host_key(
            HostKeyTrustRequest {
                host_id: host_id.clone(),
                peer: HostKeyPeer::Target,
                endpoint: endpoint.clone(),
                expected_fingerprint_sha256: server.fingerprint_sha256.clone(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("exact fingerprint confirmation is persisted");
    assert_eq!(receipt.fingerprint_sha256, server.fingerprint_sha256);
    assert_eq!(receipt.endpoint, endpoint);

    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("trusted peer accepts a later authenticated connection");
    lease.close("host key trust test complete").await.ok();
    server.shutdown().await;
}

/// 仅供 endpoint 变更竞态用例替换单个 profile，其他 SSH 行为仍使用真实 russh fixture。
struct MutableHostRepository {
    profile: StdMutex<HostConnectionProfile>,
}

impl MutableHostRepository {
    /// 保存可控的单个测试 profile，避免共享配置文件或用户主机数据。
    ///
    /// @author kongweiguang
    fn new(profile: HostConnectionProfile) -> Self {
        Self {
            profile: StdMutex::new(profile),
        }
    }

    /// 在 UI challenge 产生后更新 endpoint，精确模拟信任确认期间配置被替换。
    ///
    /// @author kongweiguang
    fn replace_profile(&self, profile: HostConnectionProfile) {
        *self
            .profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = profile;
    }
}

#[async_trait::async_trait]
impl HostConnectionRepository for MutableHostRepository {
    /// 按 HostId 返回最新快照，使 service 能发现挑战后 endpoint 已变化。
    ///
    /// @author kongweiguang
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        let profile = self
            .profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if &profile.host_id != host_id {
            return Err(AppError::not_found("测试主机不存在"));
        }
        Ok(profile)
    }
}

/// challenge 之后 profile 改指向同 key 的另一 SSH endpoint 时，旧确认不得信任新目标。
///
/// @author kongweiguang
#[tokio::test]
async fn host_key_trust_rejects_endpoint_change_after_challenge() {
    let original_server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("original loopback server starts");
    let replacement_server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("replacement loopback server starts");
    assert_eq!(
        original_server.fingerprint_sha256, replacement_server.fingerprint_sha256,
        "both isolated fixtures expose the same test SSH host key"
    );

    let (profile, credentials) = password_setup(&original_server);
    let host_id = profile.host_id.clone();
    let repository = Arc::new(MutableHostRepository::new(profile.clone()));
    let known_hosts = Arc::new(TestKnownHostsRepository::default());
    let service = Arc::new(RusshRemoteConnectionService::new(
        RusshRemoteConnectionDependencies {
            hosts: repository.clone(),
            credentials,
            known_hosts: known_hosts.clone(),
        },
    ));
    let challenge = service
        .inspect_host_key(
            host_id.clone(),
            HostKeyPeer::Target,
            CancellationToken::new(),
        )
        .await
        .expect("the original endpoint produces a live host-key challenge");
    assert_eq!(challenge.endpoint, profile.endpoint);
    assert_eq!(challenge.status, HostKeyStatus::Unknown);

    let mut replacement_profile = profile.clone();
    replacement_profile.endpoint = HostEndpoint {
        host: replacement_server.address.ip().to_string(),
        port: replacement_server.address.port(),
        username: profile.endpoint.username.clone(),
    };
    repository.replace_profile(replacement_profile.clone());

    let result = service
        .trust_host_key(
            HostKeyTrustRequest {
                host_id,
                peer: HostKeyPeer::Target,
                endpoint: challenge.endpoint.clone(),
                expected_fingerprint_sha256: challenge.fingerprint_sha256.clone(),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(result, Err(AppError::Conflict { .. })),
        "the endpoint in the stale UI confirmation must not be substituted"
    );
    assert_eq!(
        replacement_server
            .accepted_connections
            .load(Ordering::Relaxed),
        0,
        "endpoint drift is rejected before the replacement endpoint is contacted"
    );
    assert_eq!(
        original_server.accepted_connections.load(Ordering::Relaxed),
        1,
        "only the initial challenge handshake reaches the original endpoint"
    );
    assert_eq!(
        known_hosts
            .inspect(&replacement_profile.endpoint, &challenge.identity)
            .await
            .expect("replacement endpoint trust state remains readable"),
        HostKeyStatus::Unknown,
        "the stale confirmation must not write the new endpoint's trust record"
    );

    original_server.shutdown().await;
    replacement_server.shutdown().await;
}

/// 连接开始前取消必须在取密和网络操作前终止本次连接尝试。
#[tokio::test]
async fn cancelled_connection_does_not_resolve_credentials() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials.clone(), known_hosts);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = service
        .connect(host_id, ConnectionIntent::Exec, cancel)
        .await;

    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert_eq!(
        credentials
            .resolve_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    server.shutdown().await;
}

/// 连接建立后分别验证密码、公钥认证都能走运行态凭据适配器。
#[tokio::test]
async fn password_and_private_key_authentication_complete_over_russh() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");

    let (password_profile, password_credentials) = password_setup(&server);
    let password_host_id = password_profile.host_id.clone();
    let password_known_hosts = trusted_known_hosts(&server, &password_profile.endpoint);
    let password_service =
        connection_service(password_profile, password_credentials, password_known_hosts);
    let password_lease = password_service
        .connect(
            password_host_id,
            ConnectionIntent::Exec,
            CancellationToken::new(),
        )
        .await
        .expect("password authentication succeeds");
    password_lease
        .close("password test complete")
        .await
        .expect("password lease closes");

    let credential_ref = CredentialRef::new("test:private-key").expect("static ref is valid");
    let key_profile = profile_for(
        &server,
        RemoteProtocol::Ssh,
        SshAuthentication::PrivateKey {
            path: None,
            secret: Some(credential_ref),
            passphrase: None,
        },
    );
    let key_host_id = key_profile.host_id.clone();
    let key_credentials = Arc::new(TestCredentialResolver::default());
    key_credentials.insert_private_key(
        "test:private-key",
        &support::ssh_harness::test_client_private_key(),
    );
    let key_known_hosts = trusted_known_hosts(&server, &key_profile.endpoint);
    let key_service = connection_service(key_profile, key_credentials, key_known_hosts);
    let key_lease = key_service
        .connect(
            key_host_id,
            ConnectionIntent::Exec,
            CancellationToken::new(),
        )
        .await
        .expect("private-key authentication succeeds");
    key_lease
        .close("private key test complete")
        .await
        .expect("key lease closes");
    server.shutdown().await;
}

/// 单跳路由必须经已认证跳板的 direct-tcpip channel 到达目标并保留独立 host-key 校验。
#[tokio::test]
async fn jump_host_routes_exec_over_authenticated_direct_tcpip_channel() {
    let jump_server = SshServerFixture::spawn(SshServerOptions {
        allow_direct_tcpip: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("jump server starts");
    let target_server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("target server starts");
    let (mut profile, credentials) = password_setup(&target_server);
    let jump_secret = CredentialRef::new("test:jump-password").expect("static ref is valid");
    credentials.insert_password("test:jump-password", "test-password");
    profile.route.jump_hosts.push(SshJumpHost {
        endpoint: HostEndpoint {
            host: "127.0.0.1".to_owned(),
            port: jump_server.address.port(),
            username: "kerminal".to_owned(),
        },
        authentication: SshAuthentication::Password {
            secret: jump_secret,
        },
    });
    let host_id = profile.host_id.clone();
    let known_hosts = Arc::new(TestKnownHostsRepository::default());
    known_hosts.trust_fingerprint(
        &HostEndpoint {
            host: "127.0.0.1".to_owned(),
            port: jump_server.address.port(),
            username: "kerminal".to_owned(),
        },
        "ecdsa-sha2-nistp256",
        &jump_server.fingerprint_sha256,
    );
    known_hosts.trust_fingerprint(
        &profile.endpoint,
        "ecdsa-sha2-nistp256",
        &target_server.fingerprint_sha256,
    );
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("jump route connects to target");
    let mut command = lease
        .exec_stream(exec_request(), CancellationToken::new())
        .await
        .expect("target exec channel opens");
    let mut events = Vec::new();
    while let Some(event) = command
        .next_event()
        .await
        .expect("target event read succeeds")
    {
        events.push(event);
    }
    assert!(events.contains(&CommandEvent::Stdout(b"fixture-stdout".to_vec())));
    assert!(events.contains(&CommandEvent::Exited {
        code: Some(7),
        signal: None,
    }));
    command.close().await.expect("target channel closes");
    lease.close("jump route test complete").await.ok();
    target_server.shutdown().await;
    jump_server.shutdown().await;
}

/// exec stream 返回独立 stdout/stderr/退出状态，显式关闭 PTY 不触发断线重连且共享 lease 仍可用。
///
/// @author kongweiguang
#[tokio::test]
async fn exec_reports_streams_and_exit_without_closing_parent_lease() {
    let server = SshServerFixture::spawn(SshServerOptions {
        shell_probe_kind: RemoteShellKind::Bash,
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("trusted SSH connection succeeds");

    let terminal = lease
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: TerminalSize::default(),
                environment: BTreeMap::new(),
                command: None,
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("PTY and shell requests are accepted");
    assert_eq!(terminal.shell_dialect, RemoteShellDialect::Posix);
    assert!(terminal.shell_integration_enabled);
    let captured_startup = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let startup = server
                .exec_commands
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .iter()
                .find(|command| {
                    command
                        .windows(b"bash --posix --login -i".len())
                        .any(|window| window == b"bash --posix --login -i")
                })
                .cloned();
            if let Some(startup) = startup {
                break startup;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("login shell startup is sent on the exec channel");
    assert!(
        captured_startup
            .windows(b"ENV=\"$__kerminal_dir/bash-env\" bash --posix --login -i".len())
            .any(|window| window == b"ENV=\"$__kerminal_dir/bash-env\" bash --posix --login -i")
    );
    assert!(
        server
            .terminal_input
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty(),
        "startup scripts must never be written to PTY stdin"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            terminal.control.wait_exit(),
        )
        .await
        .is_err(),
        "an active interactive shell must not receive a fabricated timeout exit"
    );
    terminal
        .control
        .close()
        .await
        .expect("only the PTY channel closes");
    let terminal_exit = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        terminal.control.wait_exit(),
    )
    .await
    .expect("explicit channel close resolves wait_exit")
    .expect_err("manual close without remote status is not a successful process exit");
    assert!(!terminal_exit.is_retryable_remote_disconnect());
    assert_eq!(
        terminal_exit.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    drop(terminal);

    let mut command = lease
        .exec_stream(exec_request(), CancellationToken::new())
        .await
        .expect("new exec channel opens after PTY close");
    let mut events = Vec::new();
    while let Some(event) = command.next_event().await.expect("event read succeeds") {
        events.push(event);
    }
    assert!(events.contains(&CommandEvent::Stdout(b"fixture-stdout".to_vec())));
    assert!(events.contains(&CommandEvent::Stderr(b"fixture-stderr".to_vec())));
    assert!(events.contains(&CommandEvent::Exited {
        code: Some(7),
        signal: None,
    }));
    command.close().await.expect("exec channel closes");
    lease
        .close("exec test complete")
        .await
        .expect("lease closes");
    server.shutdown().await;
}

/// 没有 ExitStatus/ExitSignal 的远端 channel close 必须分类为 typed disconnect；读到真实 channel end 后，
/// writer shutdown 与 channel close 可安全收口，但仍须 join reader 并释放独占 lease。
///
/// @author kongweiguang
#[tokio::test]
async fn unexpected_remote_pty_close_without_exit_status_is_retryable() {
    let server = SshServerFixture::spawn(SshServerOptions {
        close_shell_without_exit_status: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("trusted SSH connection succeeds");
    let mut terminal = lease
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: TerminalSize::default(),
                environment: BTreeMap::new(),
                command: None,
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("PTY and shell requests are accepted before the server closes it");

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        terminal.control.wait_exit(),
    )
    .await
    .expect("remote channel close is observed")
    .expect_err("remote close without exit status is an error");
    assert!(error.is_retryable_remote_disconnect());
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Infrastructure,
        "retryability stays internal and does not change the MCP error code"
    );
    assert!(std::error::Error::source(&error).is_some());

    let mut drained_output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        terminal.reader.read_to_end(&mut drained_output),
    )
    .await
    .expect("reader consumes the actual remote channel close")
    .expect("reader drain completes");
    terminal
        .writer
        .shutdown()
        .await
        .expect("typed channel end makes only writer shutdown idempotent");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    terminal
        .control
        .close_until(deadline)
        .await
        .expect("confirmed remote disconnect skips dead-channel Close and joins its reader");
    lease
        .close_until(deadline, "unexpected PTY close test complete")
        .await
        .expect("the SSH lease is released");
    server.shutdown().await;
}

/// 真实 ExitStatus 保持正常退出分类；只有随后读到 channel end 才可跳过死 writer shutdown 与重复 Close。
///
/// @author kongweiguang
#[tokio::test]
async fn remote_exit_status_is_preserved_without_retryable_disconnect() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("trusted SSH connection succeeds");
    let mut terminal = lease
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: TerminalSize::default(),
                environment: BTreeMap::new(),
                command: Some("fixture normal command".to_owned()),
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("PTY exec request is accepted");

    let status = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        terminal.control.wait_exit(),
    )
    .await
    .expect("fixture sends an exit status")
    .expect("actual remote exit status is not converted into a disconnect error");
    assert_eq!(status.code, Some(7));
    assert!(!status.success);
    let mut drained_output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        terminal.reader.read_to_end(&mut drained_output),
    )
    .await
    .expect("normal exit eventually closes the remote PTY channel")
    .expect("normal exit output drains");
    terminal
        .writer
        .shutdown()
        .await
        .expect("confirmed normal channel end makes writer shutdown idempotent");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    terminal
        .control
        .close_until(deadline)
        .await
        .expect("normal exit skips a second Close and joins the reader");
    lease
        .close_until(deadline, "remote exit status test complete")
        .await
        .expect("the SSH lease is released");
    server.shutdown().await;
}

/// 真实 ExitSignal 保持正常退出分类；只有随后读到 channel end 才可跳过死 writer shutdown 与重复 Close。
///
/// @author kongweiguang
#[tokio::test]
async fn remote_exit_signal_is_preserved_without_retryable_disconnect() {
    let server = SshServerFixture::spawn(SshServerOptions {
        send_shell_exit_signal: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("trusted SSH connection succeeds");
    let mut terminal = lease
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: TerminalSize::default(),
                environment: BTreeMap::new(),
                command: None,
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("PTY and shell requests are accepted");

    let status = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        terminal.control.wait_exit(),
    )
    .await
    .expect("fixture sends an exit signal")
    .expect("actual remote exit signal is not converted into a disconnect error");
    assert_eq!(status.code, None);
    assert!(status.signal.is_some());
    assert!(!status.success);
    let mut drained_output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        terminal.reader.read_to_end(&mut drained_output),
    )
    .await
    .expect("ExitSignal is followed by actual remote channel close")
    .expect("exit-signal output drains");
    terminal
        .writer
        .shutdown()
        .await
        .expect("confirmed ExitSignal channel end makes writer shutdown idempotent");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    terminal
        .control
        .close_until(deadline)
        .await
        .expect("ExitSignal close ACK joins its reader without a second Close");
    lease
        .close_until(deadline, "remote exit signal test complete")
        .await
        .expect("the SSH lease is released");
    server.shutdown().await;
}

/// 整条 accepted TCP 被硬断时，只在 Russh Handle join 后确认关闭，并立即允许首个新连接。
///
/// @author kongweiguang
#[tokio::test]
async fn hard_tcp_disconnect_joins_parent_before_close_ack_and_reconnect() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(
            host_id.clone(),
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("trusted SSH connection succeeds");
    let mut terminal = lease
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: TerminalSize::default(),
                environment: BTreeMap::new(),
                command: None,
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("fixture accepts PTY and interactive shell");

    let marker = b"r34-single-input";
    terminal
        .writer
        .as_mut()
        .write_all(marker)
        .await
        .expect("single test input reaches the active SSH channel");
    wait_for_terminal_marker(&server, marker).await;
    server
        .hard_disconnect_accepted_connections()
        .await
        .expect("fixture drops the accepted TCP task and observes its release");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    lease
        .close_until(deadline, "hard TCP disconnect test")
        .await
        .expect("joined remote transport termination is a close ACK on the first attempt");
    let closed = service.cleanup_status();
    assert_eq!(closed.active_peer_owners, 0);
    assert_eq!(closed.reserved_route_owners, 0);
    assert_eq!(closed.pending_sessions, 0);
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            "hard TCP disconnect retry",
        )
        .await
        .expect("the same lease close remains idempotent without sending again");

    drop(terminal);
    let replacement = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("the first reconnect attempt can create a new SSH route");
    assert_eq!(server.accepted_connections.load(Ordering::Relaxed), 2);
    let captured = server
        .terminal_input
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(
        captured,
        marker.to_vec(),
        "close and reconnect do not replay input"
    );
    replacement
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(3),
            "replacement SSH route test complete",
        )
        .await
        .expect("replacement route closes normally");
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 无额外环境的原始脚本应原样走 SSH exec，即使目标方言未知也不伪造 argv 解释。
/// 捕获锁只覆盖原始 payload 断言，词法作用域结束后再等待 channel 和连接关闭。
///
/// @author kongweiguang
#[tokio::test]
async fn raw_shell_script_reaches_ssh_exec_without_dialect_reinterpretation() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("trusted SSH connection succeeds");
    let script = "printf '%s\\n' 'caller script'";
    let mut stream = lease
        .exec_shell(
            ShellScriptRequest {
                script: script.to_owned(),
                environment: BTreeMap::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("raw script is sent as SSH exec payload");
    while stream
        .next_event()
        .await
        .expect("exec stream reads successfully")
        .is_some()
    {}
    {
        let recorded = server
            .exec_commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(recorded.last().map(Vec::as_slice), Some(script.as_bytes()));
    }
    stream.close().await.expect("exec channel closes");
    lease.close("raw script test complete").await.ok();
    server.shutdown().await;
}

/// SFTP-only profile 在任何 socket 与 credential resolver 调用前拒绝 exec/terminal 能力。
#[tokio::test]
async fn sftp_only_profile_rejects_terminal_before_secret_lookup() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.protocol = RemoteProtocol::Sftp;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials.clone(), known_hosts);
    let error = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("SFTP-only host has no terminal capability");
    assert!(matches!(error, AppError::CapabilityNotSupported { .. }));
    assert_eq!(
        credentials
            .resolve_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    server.shutdown().await;
}

/// 服务器拒绝 PTY、shell 或 exec 时创建流程返回失败，不伪报已连接的会话。
#[tokio::test]
async fn server_channel_request_rejections_are_reported() {
    let pty_server = SshServerFixture::spawn(SshServerOptions {
        reject_pty: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("PTY rejection fixture starts");
    let (profile, credentials) = password_setup(&pty_server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&pty_server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("SSH authentication succeeds");
    let result = lease
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: TerminalSize::default(),
                environment: BTreeMap::new(),
                command: None,
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(
        result.is_err(),
        "server PTY rejection must reach the caller"
    );
    lease.close("PTY rejection test complete").await.ok();
    pty_server.shutdown().await;

    let shell_server = SshServerFixture::spawn(SshServerOptions {
        reject_shell: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("shell rejection fixture starts");
    let (profile, credentials) = password_setup(&shell_server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&shell_server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await
        .expect("SSH authentication succeeds");
    let result = lease
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: TerminalSize::default(),
                environment: BTreeMap::new(),
                command: None,
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(
        result.is_err(),
        "server shell rejection must reach the caller"
    );
    lease.close("shell rejection test complete").await.ok();
    shell_server.shutdown().await;

    let exec_server = SshServerFixture::spawn(SshServerOptions {
        reject_exec: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("exec rejection fixture starts");
    let (profile, credentials) = password_setup(&exec_server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&exec_server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("SSH authentication succeeds");
    let result = lease
        .exec_stream(exec_request(), CancellationToken::new())
        .await;
    assert!(
        result.is_err(),
        "server exec rejection must reach the caller"
    );
    lease.close("exec rejection test complete").await.ok();
    exec_server.shutdown().await;
}

/// 当命令输出队列已满时 close 必须取消 reader send 并在有界时间内 join。
#[tokio::test]
async fn close_cancels_reader_blocked_on_bounded_output_queue() {
    let server = SshServerFixture::spawn(SshServerOptions {
        exec_burst_chunks: 64,
        ..SshServerOptions::default()
    })
    .await
    .expect("burst fixture starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("trusted SSH connection succeeds");
    let mut command = lease
        .exec_stream(exec_request(), CancellationToken::new())
        .await
        .expect("exec channel opens");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    command
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .expect("cancelled reader joins cleanly before the shared deadline");
    lease.close("backpressure test complete").await.ok();
    server.shutdown().await;
}

/// 验证初始 deadline 已耗尽时会取消本 channel，后续仍能复用 stream owner 完成 join。
#[tokio::test]
async fn expired_command_stream_close_can_be_retried() {
    let server = SshServerFixture::spawn(SshServerOptions {
        exec_burst_chunks: 64,
        ..SshServerOptions::default()
    })
    .await
    .expect("burst fixture starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("trusted SSH connection succeeds");
    let mut command = lease
        .exec_stream(exec_request(), CancellationToken::new())
        .await
        .expect("exec channel opens");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let expired = command
        .close_until(tokio::time::Instant::now())
        .await
        .expect_err("expired close must report timeout and retain cleanup owner");
    assert_eq!(
        expired.public_code(),
        kerminal_application::PublicErrorCode::Timeout
    );
    command
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .expect("retained reader task and channel close can be joined on retry");
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            "expired command stream close retry complete",
        )
        .await
        .expect("parent lease closes");
    server.shutdown().await;
}

/// 验证取消后已排队事件可排空至 EOF，且关闭 exec channel 后同一 lease 仍可执行后续命令。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_exec_channel_does_not_close_parent_lease() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("trusted SSH connection succeeds");

    let cancellation = CancellationToken::new();
    let mut cancelled = lease
        .exec_shell(
            ShellScriptRequest {
                script: "sleep 30".to_owned(),
                environment: BTreeMap::new(),
            },
            cancellation.clone(),
        )
        .await
        .expect("long running command channel opens");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancellation.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while cancelled
            .next_event()
            .await
            .expect("cancelled command reader must return a valid application result")
            .is_some()
        {}
    })
    .await
    .expect("cancel must drain buffered events and close the command reader");
    cancelled
        .close()
        .await
        .expect("cancelled command reader joins");

    let mut next = lease
        .exec_stream(exec_request(), CancellationToken::new())
        .await
        .expect("parent lease remains usable after command cancellation");
    let mut saw_stdout = false;
    while let Some(event) = next
        .next_event()
        .await
        .expect("subsequent command event read succeeds")
    {
        if matches!(&event, CommandEvent::Stdout(bytes) if bytes.as_slice() == b"fixture-stdout") {
            saw_stdout = true;
        }
        if matches!(event, CommandEvent::Exited { .. }) {
            break;
        }
    }
    next.close()
        .await
        .expect("subsequent command channel closes");
    assert!(
        saw_stdout,
        "the parent lease must still carry command output"
    );
    lease
        .close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            "command cancellation test complete",
        )
        .await
        .expect("parent lease closes");
    server.shutdown().await;
}

/// 结构化 SSH 初始命令保持 argv 边界，并由可信 POSIX 方言编码为安全 exec payload。
/// payload 比较保持在同一临界区，离开词法作用域后才能异步回收 channel 与独占 lease。
///
/// @author kongweiguang
#[tokio::test]
async fn ssh_initial_process_is_encoded_from_structured_arguments() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let request = ExecRequest {
        program: "printf".to_owned(),
        args: vec![
            "arg with spaces".to_owned(),
            "quote' dollar$; $(false)".to_owned(),
        ],
        environment: BTreeMap::new(),
        working_directory: None,
    };
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: None,
        command: Some(SshInitialCommand::Process {
            program: request.program.clone(),
            args: request.args.clone(),
        }),
    });

    let transport = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .expect("structured SSH initial process opens");
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        transport.control.wait_exit(),
    )
    .await
    .expect("fixture completes the initial process")
    .expect("fixture exit status is preserved");
    assert_eq!(status.code, Some(7));

    let expected_payload = encode_exec_command(&request, RemoteShellDialect::Posix)
        .expect("POSIX encoder accepts the structured arguments");
    {
        let recorded = server
            .exec_commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(
            recorded.last().map(Vec::as_slice),
            Some(expected_payload.as_bytes()),
            "the SSH adapter must preserve argument data and use the shared encoder"
        );
    }

    transport
        .control
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(3))
        .await
        .expect("initial process channel and its exclusive lease close");
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// Shell 初始命令保留明确的原始脚本语义，不因目标 dialect 未知而被重新解释。
/// 用词法边界限定捕获锁的存活期，保留原始断言且不让关闭等待持有同步锁。
///
/// @author kongweiguang
#[tokio::test]
async fn ssh_initial_shell_command_preserves_raw_payload() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let script = "printf '%s\\n' 'caller supplied shell text'";
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: None,
        command: Some(SshInitialCommand::Shell(script.to_owned())),
    });

    let transport = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .expect("raw shell command remains available for unknown dialect");
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        transport.control.wait_exit(),
    )
    .await
    .expect("fixture completes the raw shell command")
    .expect("fixture exit status is preserved");
    assert_eq!(status.code, Some(7));

    {
        let recorded = server
            .exec_commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(recorded.last().map(Vec::as_slice), Some(script.as_bytes()));
    }
    transport
        .control
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(3))
        .await
        .expect("raw shell command channel and its exclusive lease close");
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 结构化 Process 在方言未知时明确失败，并回收已经建立的 PTY 与 SSH lease。
///
/// @author kongweiguang
#[tokio::test]
async fn ssh_initial_process_rejects_unknown_shell_dialect() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (mut profile, credentials) = password_setup(&server);
    profile.shell_dialect = RemoteShellDialect::Unknown;
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: None,
        command: Some(SshInitialCommand::Process {
            program: "printf".to_owned(),
            args: vec!["literal argument".to_owned()],
        }),
    });

    let error = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .err()
        .expect("structured argv cannot be encoded without a trusted shell dialect");
    let (primary_error, cleanup_owner) = error.into_parts();
    assert_eq!(
        primary_error.public_code(),
        kerminal_application::PublicErrorCode::Unsupported
    );
    assert!(
        cleanup_owner.is_none(),
        "successful rollback releases the lease"
    );
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 空程序在结构化命令边界明确拒绝，不能降级成 shell 字符串执行。
///
/// @author kongweiguang
#[tokio::test]
async fn ssh_initial_process_rejects_empty_program() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: None,
        command: Some(SshInitialCommand::Process {
            program: String::new(),
            args: Vec::new(),
        }),
    });

    let error = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .err()
        .expect("an empty structured program is invalid");
    let (primary_error, cleanup_owner) = error.into_parts();
    assert_eq!(
        primary_error.public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert!(
        cleanup_owner.is_none(),
        "successful rollback releases the lease"
    );
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 普通 SSH SessionTransport 关闭时应同时关闭本次 channel 与专属 peer lease。
///
/// @author kongweiguang
#[tokio::test]
async fn ordinary_ssh_factory_close_releases_its_lease() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let target = ssh_session_target(host_id);
    let transport = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .expect("ordinary SSH PTY opens");

    wait_for_counter(&server.pty_requests, 1).await;
    transport
        .control
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(3))
        .await
        .expect("channel and its exclusive SSH lease close");
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 取消发生在 server 已收到 PTY request 后，factory 仍须清理未发布 channel 和 lease。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_ordinary_ssh_open_releases_its_lease() {
    let server = SshServerFixture::spawn(SshServerOptions {
        pty_request_delay: std::time::Duration::from_millis(250),
        ..SshServerOptions::default()
    })
    .await
    .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let target = ssh_session_target(host_id);
    let cancel = CancellationToken::new();
    let open_cancel = cancel.clone();
    let open_task = tokio::spawn(async move {
        factory
            .open(&target, TerminalSize::default(), open_cancel)
            .await
    });

    wait_for_counter(&server.pty_requests, 1).await;
    cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(4), open_task)
        .await
        .expect("cancelled factory open finishes after bounded cleanup")
        .expect("factory open task does not panic");
    let error = result
        .err()
        .expect("cancelled PTY open must not publish a transport");
    let (primary_error, cleanup_owner) = error.into_parts();
    assert_eq!(
        primary_error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    assert!(
        !primary_error.is_retryable_remote_disconnect(),
        "an explicitly cancelled open is never a reconnect signal"
    );
    assert!(
        cleanup_owner.is_none(),
        "successful rollback needs no owner"
    );
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 已耗尽的关闭期限会取消当前资源但保留 owner，后续 deadline 能完成 channel 与 lease 回收。
///
/// @author kongweiguang
#[tokio::test]
async fn expired_ordinary_ssh_close_retains_owner_for_retry() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let target = ssh_session_target(host_id);
    let transport = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .expect("ordinary SSH PTY opens");

    let expired = transport
        .control
        .close_until(tokio::time::Instant::now())
        .await
        .expect_err("expired deadline reports timeout and starts cancellation");
    assert_eq!(
        expired.public_code(),
        kerminal_application::PublicErrorCode::Timeout
    );
    transport
        .control
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(3))
        .await
        .expect("the same control retains both owners for a later join");
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// 关闭一个普通 pane 的 SSH lease 不得中断同一主机上另一个 pane 的 PTY。
///
/// @author kongweiguang
#[tokio::test]
async fn closing_one_ordinary_ssh_pane_keeps_sibling_pty_usable() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback server starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let target = ssh_session_target(host_id);
    let first = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .expect("first ordinary SSH PTY opens");
    let mut sibling = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .expect("sibling ordinary SSH PTY opens on an independent lease");
    wait_for_counter(&server.pty_requests, 2).await;

    first
        .control
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(3))
        .await
        .expect("first pane closes only its channel and lease");
    wait_for_counter(&server.closed_connections, 1).await;
    let marker = b"sibling-pty-still-usable";
    sibling
        .writer
        .write_all(marker)
        .await
        .expect("sibling writer remains connected");
    sibling
        .writer
        .flush()
        .await
        .expect("sibling input reaches the SSH transport");
    wait_for_terminal_marker(&server, marker).await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            sibling.control.wait_exit()
        )
        .await
        .is_err()
    );

    sibling
        .control
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(3))
        .await
        .expect("sibling pane closes independently");
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

/// PTY 拒绝必须触发未发布连接的回收，不能把 lease 留在后台。
///
/// @author kongweiguang
#[tokio::test]
async fn rejected_ordinary_ssh_pty_releases_its_lease() {
    let server = SshServerFixture::spawn(SshServerOptions {
        reject_pty: true,
        ..SshServerOptions::default()
    })
    .await
    .expect("PTY rejection fixture starts");
    let (profile, credentials) = password_setup(&server);
    let host_id = profile.host_id.clone();
    let known_hosts = trusted_known_hosts(&server, &profile.endpoint);
    let service = connection_service(profile, credentials, known_hosts);
    let factory = SshSessionTransportFactory::new(service);
    let target = ssh_session_target(host_id);

    let error = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .err()
        .expect("server PTY rejection must not publish a transport");
    let (primary_error, cleanup_owner) = error.into_parts();
    assert_eq!(
        primary_error.public_code(),
        kerminal_application::PublicErrorCode::Unsupported
    );
    assert!(
        cleanup_owner.is_none(),
        "successful rollback needs no owner"
    );
    wait_for_all_fixture_connections_closed(&server).await;
    server.shutdown().await;
}

struct RetryableOpenFailureConnections {
    handle: Arc<RetryableOpenFailureHandle>,
}

struct RetryableOpenFailureHandle {
    host_id: HostId,
    close_attempts: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RemoteConnectionService for RetryableOpenFailureConnections {
    /// 为失败清理用例返回一个隔离 lease，避免测试连接任何真实主机。
    /// @author kongweiguang
    async fn connect(
        &self,
        host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        if host_id != self.handle.host_id {
            return Err(AppError::not_found("test host not found"));
        }
        let inner: Arc<dyn RemoteConnectionHandle> = self.handle.clone();
        Ok(RemoteConnectionLease::new(inner))
    }

    /// 此测试不经过 host-key 流程，意外调用时明确拒绝。
    /// @author kongweiguang
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("test host-key inspection is unused"))
    }

    /// 此测试不保存 host-key 信任，意外调用时明确拒绝。
    /// @author kongweiguang
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("test host-key trust is unused"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

#[async_trait::async_trait]
impl RemoteConnectionHandle for RetryableOpenFailureHandle {
    /// 返回当前测试 lease 唯一允许访问的 host id。
    /// @author kongweiguang
    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// 仅开放本测试所需的终端能力。
    /// @author kongweiguang
    fn capabilities(&self) -> ConnectionCapabilities {
        ConnectionCapabilities {
            terminal: true,
            exec: false,
            sftp: false,
            tunnel: false,
            launch_rdp: false,
        }
    }

    /// 方言不参与本测试的 setup error 路径。
    /// @author kongweiguang
    fn shell_dialect(&self) -> RemoteShellDialect {
        RemoteShellDialect::Unknown
    }

    /// 注入 PTY setup 主错误，使 factory 进入 lease rollback 分支。
    /// @author kongweiguang
    async fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        Err(AppError::unsupported("injected PTY setup rejection"))
    }

    /// 命令能力不属于本测试，调用时明确拒绝。
    /// @author kongweiguang
    async fn exec_stream(
        &self,
        _request: ExecRequest,
        _cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        Err(AppError::unsupported("test exec stream is unused"))
    }

    /// 原始 shell 命令能力不属于本测试，调用时明确拒绝。
    /// @author kongweiguang
    async fn exec_shell(
        &self,
        _request: ShellScriptRequest,
        _cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        Err(AppError::unsupported("test shell command is unused"))
    }

    /// SFTP 能力不属于本测试，调用时明确拒绝。
    /// @author kongweiguang
    async fn open_sftp(
        &self,
        _request: OpenSftpRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        Err(AppError::unsupported("test SFTP channel is unused"))
    }

    /// 转发能力不属于本测试，调用时明确拒绝。
    /// @author kongweiguang
    async fn open_tunnel(
        &self,
        _request: TunnelRequest,
        _cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        Err(AppError::unsupported("test tunnel channel is unused"))
    }

    /// 远端 listener 能力不属于本测试，调用时明确拒绝。
    /// @author kongweiguang
    async fn request_remote_forward(
        &self,
        _request: RemoteForwardRequest,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        Err(AppError::unsupported("test remote forward is unused"))
    }

    /// 首次清理模拟可恢复超时，owner 重试后成功以证明 lease 可被继续接管。
    /// @author kongweiguang
    async fn close_until(&self, _deadline: tokio::time::Instant, _reason: &str) -> AppResult<()> {
        if self.close_attempts.fetch_add(1, Ordering::Relaxed) == 0 {
            Err(AppError::timeout("injected first lease cleanup timeout"))
        } else {
            Ok(())
        }
    }
}

/// 首次 rollback 失败时，factory 将 lease-only cleanup owner 交给 manager 可重试。
///
/// @author kongweiguang
#[tokio::test]
async fn failed_ordinary_ssh_open_returns_retryable_lease_owner() {
    let host_id = HostId::new();
    let close_attempts = Arc::new(AtomicUsize::new(0));
    let handle = Arc::new(RetryableOpenFailureHandle {
        host_id: host_id.clone(),
        close_attempts: Arc::clone(&close_attempts),
    });
    let service: Arc<dyn RemoteConnectionService> =
        Arc::new(RetryableOpenFailureConnections { handle });
    let factory = SshSessionTransportFactory::new(service);
    let target = ssh_session_target(host_id);

    let error = factory
        .open(&target, TerminalSize::default(), CancellationToken::new())
        .await
        .err()
        .expect("injected PTY setup failure must not publish a transport");
    let (primary_error, cleanup_owner) = error.into_parts();
    assert_eq!(
        primary_error.public_code(),
        kerminal_application::PublicErrorCode::Unsupported
    );
    let cleanup_owner = cleanup_owner.expect("failed rollback retains its lease owner");
    assert_eq!(close_attempts.load(Ordering::Relaxed), 1);

    cleanup_owner
        .close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
        .await
        .expect("the manager-held owner can retry lease close");
    assert_eq!(close_attempts.load(Ordering::Relaxed), 2);
}
