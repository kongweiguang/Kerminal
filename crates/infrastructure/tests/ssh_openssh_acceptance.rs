//! 可选的真实 OpenSSH 互通验收；所有 fixture 路径必须由测试任务单独创建。
//!
//! 运行前提供 `KERM_ACCEPT_SSH_TARGET`、`KERM_ACCEPT_SSH_JUMP_ENDPOINTS`、
//! `KERM_ACCEPT_SSH_USERNAME`、`KERM_ACCEPT_SSH_PRIVATE_KEY_FILE` 和
//! `KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE`，再运行 `cargo test -p kerminal-infrastructure
//! --test ssh_openssh_acceptance -- --ignored`。known_hosts 会复制到独立临时根目录。
//!
//! @author kongweiguang

mod support;

use std::{
    collections::BTreeMap, error::Error, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CommandEvent, ConnectionIntent, CredentialResolver, HostConnectionRepository,
        KnownHostsRepository, RemoteConnectionLease, RemoteConnectionService, ResolvedCredential,
        ShellScriptRequest, TerminalOpenRequest,
    },
    sessions::{SessionTransport, SessionTransportControl},
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol, RemoteShellDialect,
        SshAuthentication, SshJumpHost, SshRoute,
    },
    identity::HostId,
    sessions::TerminalSize,
};
use kerminal_infrastructure::{
    connections::{
        KnownHostsStore, RusshRemoteConnectionDependencies, RusshRemoteConnectionService,
    },
    storage::StorageCoordinator,
};
use tokio::io::AsyncReadExt;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use support::ssh_harness::StaticHostRepository;

const KEY_REFERENCE: &str = "acceptance:openssh-client-key";
const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// 保存测试任务提供的 loopback 地址与私有临时材料，不读取系统 SSH 配置。
struct OpenSshFixtureConfig {
    target: SocketAddr,
    jumps: Vec<SocketAddr>,
    username: String,
    private_key_file: PathBuf,
    known_hosts_file: PathBuf,
}

impl OpenSshFixtureConfig {
    /// 仅从显式环境变量装载 fixture 信息；错误文案不回显 key 内容或变量值。
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let target = std::env::var("KERM_ACCEPT_SSH_TARGET")?.parse()?;
        let jumps = std::env::var("KERM_ACCEPT_SSH_JUMP_ENDPOINTS")?
            .split(',')
            .map(|endpoint| endpoint.trim().parse::<SocketAddr>())
            .collect::<Result<Vec<_>, _>>()?;
        let username = std::env::var("KERM_ACCEPT_SSH_USERNAME")?;
        let private_key_file = required_path("KERM_ACCEPT_SSH_PRIVATE_KEY_FILE")?;
        let known_hosts_file = required_path("KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE")?;
        if jumps.len() < 2 {
            return Err("the isolated SSH fixture must provide at least two jump endpoints".into());
        }
        if username.trim().is_empty() {
            return Err("the isolated SSH fixture username must not be empty".into());
        }
        Ok(Self {
            target,
            jumps,
            username,
            private_key_file,
            known_hosts_file,
        })
    }

    /// 把 loopback socket 地址转换成应用层目标，不拼接 SSH 命令或路径。
    fn endpoint(&self, address: SocketAddr) -> HostEndpoint {
        HostEndpoint {
            host: address.ip().to_string(),
            port: address.port(),
            username: self.username.clone(),
        }
    }
}

/// 读取必须由任务 fixture 提供的路径变量，不把路径值放进缺失变量错误。
fn required_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    std::env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("required OpenSSH fixture variable {name} is missing"),
        )
        .into()
    })
}

/// 在任务临时凭据解析器中保留 key 的零化副本，不落入主机配置或用户 vault。
struct FixtureKeyResolver {
    reference: CredentialRef,
    private_key: Zeroizing<Vec<u8>>,
}

#[async_trait]
impl CredentialResolver for FixtureKeyResolver {
    /// 只为已知测试引用返回私钥副本，并在取消时拒绝解析。
    async fn resolve(
        &self,
        reference: &CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled(
                "OpenSSH acceptance credential lookup cancelled",
            ));
        }
        if reference != &self.reference {
            return Err(AppError::not_found(
                "OpenSSH acceptance credential is unavailable",
            ));
        }
        Ok(ResolvedCredential::PrivateKey(Zeroizing::new(
            self.private_key.to_vec(),
        )))
    }
}

/// 确保验收断言失败时仍异步关闭连接，避免遗留 russh 后台 Handle。
struct LeaseGuard {
    lease: Option<RemoteConnectionLease>,
}

impl LeaseGuard {
    /// 接管 lease，使测试退出路径能够显式使用绝对 deadline 收尾。
    fn new(lease: RemoteConnectionLease) -> Self {
        Self { lease: Some(lease) }
    }

    /// 访问当前仍由 guard 所有的连接租约。
    fn lease(&self) -> &RemoteConnectionLease {
        self.lease.as_ref().expect("lease guard is active")
    }

    /// 使用调用方给出的共享关闭期限；失败时保留 lease 供 Drop 安排再次清理。
    async fn close(&mut self, deadline: Instant, reason: &str) -> AppResult<()> {
        if let Some(lease) = &self.lease {
            lease.close_until(deadline, reason).await?;
            self.lease = None;
        }
        Ok(())
    }
}

impl Drop for LeaseGuard {
    /// 测试中途失败时安排有界清理；正常路径仍必须显式等待 `close` 完成。
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = lease
                    .close_until(
                        Instant::now() + Duration::from_secs(10),
                        "acceptance guard drop",
                    )
                    .await;
            });
        }
    }
}

/// 确保 PTY transport 在断言失败时也触发 channel close，而不是依赖字段 drop。
struct TransportGuard {
    transport: Option<SessionTransport>,
}

impl TransportGuard {
    /// 接管 provider 返回的 owned transport。
    fn new(transport: SessionTransport) -> Self {
        Self {
            transport: Some(transport),
        }
    }

    /// 访问仍处于打开状态的 transport reader 和生命周期控制器。
    fn transport(&mut self) -> &mut SessionTransport {
        self.transport.as_mut().expect("transport guard is active")
    }

    /// 在共享期限内关闭一个 PTY channel，并保留失败后的重试所有权。
    async fn close(&mut self, deadline: Instant) -> AppResult<()> {
        if let Some(transport) = &self.transport {
            transport.control.close_until(deadline).await?;
            self.transport = None;
        }
        Ok(())
    }
}

impl Drop for TransportGuard {
    /// 验收断言提前返回时仍唤醒 PTY reader，并等待独立清理任务持有 channel owner。
    fn drop(&mut self) {
        let Some(transport) = self.transport.take() else {
            return;
        };
        let control: Arc<dyn SessionTransportControl> = Arc::clone(&transport.control);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = control
                    .close_until(Instant::now() + Duration::from_secs(10))
                    .await;
            });
        }
    }
}

/// 按一跳或两跳配置构建 production SSH service；known_hosts 始终指向临时存储根。
fn connection_service(
    fixture: &OpenSshFixtureConfig,
    jump_count: usize,
    credentials: Arc<dyn CredentialResolver>,
    known_hosts: Arc<dyn KnownHostsRepository>,
) -> (Arc<dyn RemoteConnectionService>, HostId) {
    let reference = CredentialRef::new(KEY_REFERENCE).expect("static fixture ref is valid");
    let authentication = SshAuthentication::PrivateKey {
        path: None,
        secret: Some(reference),
        passphrase: None,
    };
    let jump_hosts = fixture
        .jumps
        .iter()
        .take(jump_count)
        .copied()
        .map(|address| SshJumpHost {
            endpoint: fixture.endpoint(address),
            authentication: authentication.clone(),
        })
        .collect();
    let profile = HostConnectionProfile {
        host_id: HostId::new(),
        protocol: RemoteProtocol::Ssh,
        endpoint: fixture.endpoint(fixture.target),
        authentication: Some(authentication),
        rdp_password: None,
        route: SshRoute {
            proxy: None,
            jump_hosts,
        },
        shell_dialect: RemoteShellDialect::Unknown,
        connect_timeout_ms: 20_000,
        keepalive_interval_ms: None,
    };
    let host_id = profile.host_id.clone();
    let hosts: Arc<dyn HostConnectionRepository> = Arc::new(StaticHostRepository::new(profile));
    let service: Arc<dyn RemoteConnectionService> = Arc::new(RusshRemoteConnectionService::new(
        RusshRemoteConnectionDependencies {
            hosts,
            credentials,
            known_hosts,
        },
    ));
    (service, host_id)
}

/// 读取 SSH exec 的原始 stdout/stderr，并要求服务端真实发送 exit-status 后再关闭 channel。
async fn collect_script(
    lease: &RemoteConnectionLease,
    script: &str,
) -> Result<(Vec<u8>, Vec<u8>, Option<u32>), Box<dyn Error>> {
    let mut stream = lease
        .exec_shell(
            ShellScriptRequest {
                script: script.to_owned(),
                environment: BTreeMap::new(),
            },
            CancellationToken::new(),
        )
        .await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    let read_result = async {
        while let Some(event) = stream.next_event().await? {
            match event {
                CommandEvent::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                CommandEvent::Stderr(bytes) => stderr.extend_from_slice(&bytes),
                CommandEvent::Exited { code, signal } => {
                    exit_code = code;
                    if signal.is_some() {
                        return Err(AppError::infrastructure(
                            "OpenSSH acceptance command exited by signal",
                        ));
                    }
                    break;
                }
            }
        }
        Ok::<(), AppError>(())
    }
    .await;
    let close_result = stream.close().await;
    read_result?;
    close_result?;
    Ok((stdout, stderr, exit_code))
}

/// 在 PTY 流中读取一个有限 marker，用于确认关闭同 lease 的另一个会话没有切断输出。
async fn read_until_marker(
    transport: &mut SessionTransport,
    marker: &[u8],
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut output = Vec::new();
    let read_result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut chunk = [0_u8; 2048];
        while !output.windows(marker.len()).any(|window| window == marker) {
            let count = transport.reader.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..count]);
        }
        Ok::<(), std::io::Error>(())
    })
    .await?;
    read_result?;
    Ok(output)
}

/// 建立真实 OpenSSH 单跳与双跳 SSH 链路，并验证 exec、PTY 隔离和取消后的显式收尾。
#[tokio::test]
#[ignore = "requires the task-private OpenSSH fixture; see this file's environment contract"]
async fn openssh_routes_exec_pty_isolation_and_cancellation() -> Result<(), Box<dyn Error>> {
    let fixture = OpenSshFixtureConfig::from_environment()?;
    let key_bytes = Zeroizing::new(std::fs::read(&fixture.private_key_file)?);
    if key_bytes.is_empty() {
        return Err("the isolated SSH fixture private key is empty".into());
    }
    let credentials: Arc<dyn CredentialResolver> = Arc::new(FixtureKeyResolver {
        reference: CredentialRef::new(KEY_REFERENCE).expect("static fixture ref is valid"),
        private_key: key_bytes,
    });

    let known_hosts_root = tempfile::tempdir()?;
    std::fs::copy(
        &fixture.known_hosts_file,
        known_hosts_root.path().join(KNOWN_HOSTS_FILE),
    )?;
    let known_hosts: Arc<dyn KnownHostsRepository> = Arc::new(KnownHostsStore::new(
        StorageCoordinator::new(known_hosts_root.path()),
        false,
    ));

    for jump_count in [1, 2] {
        let (service, host_id) = connection_service(
            &fixture,
            jump_count,
            Arc::clone(&credentials),
            Arc::clone(&known_hosts),
        );
        let lease = service
            .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
            .await?;
        let mut lease = LeaseGuard::new(lease);
        let (stdout, stderr, exit_code) = collect_script(
            lease.lease(),
            "printf 'KERM_OPENSSH_STDOUT'; printf 'KERM_OPENSSH_STDERR' >&2; exit 23",
        )
        .await?;
        lease
            .close(
                Instant::now() + Duration::from_secs(20),
                "OpenSSH route exec acceptance complete",
            )
            .await
            .map_err(|error| {
                std::io::Error::other(format!(
                    "OpenSSH route {jump_count} exec lease close failed: {error:?}"
                ))
            })?;
        assert!(
            stdout
                .windows(b"KERM_OPENSSH_STDOUT".len())
                .any(|window| { window == b"KERM_OPENSSH_STDOUT" })
        );
        assert!(
            stderr
                .windows(b"KERM_OPENSSH_STDERR".len())
                .any(|window| { window == b"KERM_OPENSSH_STDERR" })
        );
        assert_eq!(exit_code, Some(23));
    }

    let (service, host_id) = connection_service(
        &fixture,
        2,
        Arc::clone(&credentials),
        Arc::clone(&known_hosts),
    );
    let lease = service
        .connect(
            host_id,
            ConnectionIntent::Terminal,
            CancellationToken::new(),
        )
        .await?;
    let mut lease = LeaseGuard::new(lease);
    let terminal_size = TerminalSize::new(80, 24, 0, 0).expect("valid terminal size");
    let terminal_a = lease
        .lease()
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: terminal_size,
                environment: BTreeMap::new(),
                command: Some("sleep 30".to_owned()),
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await?;
    let mut terminal_a = TransportGuard::new(terminal_a);
    let terminal_b = lease
        .lease()
        .open_terminal(
            TerminalOpenRequest {
                term: "xterm-256color".to_owned(),
                size: terminal_size,
                environment: BTreeMap::new(),
                command: Some(
                    "printf 'KERM_PTY_B_START'; sleep 1; printf 'KERM_PTY_B_ALIVE'".to_owned(),
                ),
                program: None,
                args: Vec::new(),
                working_directory: None,
            },
            CancellationToken::new(),
        )
        .await?;
    let mut terminal_b = TransportGuard::new(terminal_b);
    terminal_a
        .close(Instant::now() + Duration::from_secs(10))
        .await?;
    let terminal_output = read_until_marker(terminal_b.transport(), b"KERM_PTY_B_ALIVE").await?;
    assert!(
        terminal_output
            .windows(b"KERM_PTY_B_ALIVE".len())
            .any(|window| window == b"KERM_PTY_B_ALIVE")
    );
    terminal_b
        .close(Instant::now() + Duration::from_secs(10))
        .await?;

    let cancellation = CancellationToken::new();
    let mut command = lease
        .lease()
        .exec_shell(
            ShellScriptRequest {
                script: "sleep 30".to_owned(),
                environment: BTreeMap::new(),
            },
            cancellation.clone(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    cancellation.cancel();
    let event_result = tokio::time::timeout(Duration::from_secs(10), command.next_event()).await;
    let command_close = command.close().await;
    let lease_close = lease
        .close(
            Instant::now() + Duration::from_secs(20),
            "OpenSSH cancellation acceptance complete",
        )
        .await
        .map_err(|error| {
            std::io::Error::other(format!(
                "OpenSSH cancellation lease close failed: {error:?}"
            ))
        });
    assert!(
        event_result.is_ok(),
        "cancelled command reader must unblock"
    );
    command_close?;
    lease_close?;
    Ok(())
}
