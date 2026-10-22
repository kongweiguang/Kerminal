//! 运维能力的隔离 OpenSSH 端到端验收：服务器快照、四类隧道和 tmux 生命周期。
//!
//! 仅连接任务私有的 `127.0.0.1:22661` fixture；环境变量为
//! `KERM_ACCEPT_SSH_TARGET`、`KERM_ACCEPT_SSH_USERNAME`、
//! `KERM_ACCEPT_SSH_PRIVATE_KEY_FILE`、`KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE`。
//! 普通验收：`cargo +1.98.0 test --locked -p kerminal-infrastructure --test
//! operations_ssh_acceptance operations_server_info_forwarding_and_tmux_lifecycle -- --ignored --nocapture`。
//! 断连验收单独运行：`cargo +1.98.0 test --locked -p kerminal-infrastructure --test
//! operations_ssh_acceptance operations_forwarding_reports_fixture_disconnect_and_releases_listener -- --ignored --nocapture`。
//! 看到 `OPERATIONS_SSH_FAULT_READY` 后，外部控制者只停止私有 fixture 的 `target`，回读状态，
//! 再向 marker 目录写 `stopped:<RUN_ID>` 到 `signal.txt`。用例退出后外部重新启动并回读 fixture；
//! 本测试自身不启停 sshd。
//!
//! @author kongweiguang

mod support;

use std::{
    error::Error,
    fmt::Write as _,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use kerminal_application::{
    connections::{CredentialResolver, RemoteConnectionService},
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        ContainerStageCleanupCapability, ContainerStageCleanupRequest, OperationsCommandExecutor,
        OperationsCommandStream, OperationsService, ServerInfoQuery, ServerInfoTier,
        TmuxCreateRequest,
    },
    preferences::ProfileRepository,
    sessions::{SessionService, SessionTransportFactory},
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol, RemoteShellDialect,
        SshAuthentication, SshRoute,
    },
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, ForwardState, PortForwardConfig, ProxyApplyScope,
        ProxyProtocol, RemoteAccessScope, ServerInfoTarget, TmuxTarget,
    },
    sessions::{SessionOwner, TerminalSize},
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore,
    connections::KnownHostsStore,
    local_pty::LocalPtySessionTransportFactory,
    operations::{
        SshPortForwardRuntime, StructuredOperationsCommandExecutor, SystemServerInfoCollector,
    },
    sessions::{SessionManager, SessionRuntimeConfig},
    storage::StorageCoordinator,
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    runtime::Handle,
    time::{Instant, sleep},
};
use tokio_socks::tcp::socks5::Socks5Stream;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use support::ssh_harness::{TestCredentialResolver, connection_service};

const SSH_FIXTURE_TARGET: &str = "127.0.0.1:22661";
const SSH_FIXTURE_KEY_REFERENCE: &str = "acceptance:operations-ssh-key";
const MAX_REMOTE_OUTPUT_BYTES: usize = 16 * 1024;
const OPERATIONS_COMMAND_TIMEOUT: Duration = Duration::from_secs(12);

/// 仅保存任务私有 SSH fixture 的连接端点和临时文件位置。
struct OpenSshFixtureConfig {
    target: SocketAddr,
    username: String,
    private_key_file: PathBuf,
    known_hosts_file: PathBuf,
}

impl OpenSshFixtureConfig {
    /// 只接受任务指定 loopback sshd 与其临时凭据，防止忽略测试误连用户主机。
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let target: SocketAddr = std::env::var("KERM_ACCEPT_SSH_TARGET")?.parse()?;
        let username = std::env::var("KERM_ACCEPT_SSH_USERNAME")?;
        let private_key_file = required_path("KERM_ACCEPT_SSH_PRIVATE_KEY_FILE")?;
        let known_hosts_file = required_path("KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE")?;
        let expected: SocketAddr = SSH_FIXTURE_TARGET.parse()?;
        if target != expected || !target.ip().is_loopback() {
            return Err(io_error(
                "operations acceptance requires the task-private loopback SSH fixture",
            )
            .into());
        }
        if username.trim().is_empty() {
            return Err(io_error("the isolated SSH fixture username is empty").into());
        }
        Ok(Self {
            target,
            username,
            private_key_file,
            known_hosts_file,
        })
    }

    /// 构造只含 loopback 地址和用户的连接端点，不读取系统 SSH 配置。
    fn endpoint(&self) -> HostEndpoint {
        HostEndpoint {
            host: self.target.ip().to_string(),
            port: self.target.port(),
            username: self.username.clone(),
        }
    }
}

/// 读取 fixture 路径；错误不回显环境变量值或凭据正文。
fn required_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    std::env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        io_error(&format!(
            "required isolated SSH fixture path {name} is missing"
        ))
        .into()
    })
}

/// 将固定安全文案转换为测试错误，便于 ignored acceptance 在缺前置条件时明确失败。
fn io_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_owned())
}

/// 用临时 known_hosts、凭据 resolver、配置根和唯一 SessionManager 装配真实运维用例。
struct OperationsFixture {
    operations: OperationsService,
    commands: Arc<StructuredOperationsCommandExecutor>,
    tmux_observer: Arc<TmuxCommandObserver>,
    sessions: Arc<SessionManager>,
    host_id: HostId,
    _known_hosts_root: tempfile::TempDir,
    _configuration_root: tempfile::TempDir,
}

impl OperationsFixture {
    /// 复制可信主机记录到测试私有根目录，并让真实 tmux 用例经过可观测的同一执行端口。
    fn new(config: &OpenSshFixtureConfig) -> Result<Self, Box<dyn Error>> {
        let private_key = Zeroizing::new(std::fs::read(&config.private_key_file)?);
        if private_key.is_empty() {
            return Err(io_error("the isolated SSH fixture private key is empty").into());
        }
        let resolver = Arc::new(TestCredentialResolver::default());
        resolver.insert_private_key(SSH_FIXTURE_KEY_REFERENCE, &private_key);
        let credentials: Arc<dyn CredentialResolver> = resolver;

        let known_hosts_root = tempfile::tempdir()?;
        std::fs::copy(
            &config.known_hosts_file,
            known_hosts_root.path().join("known_hosts"),
        )?;
        let known_hosts = Arc::new(KnownHostsStore::new(
            StorageCoordinator::new(known_hosts_root.path()),
            false,
        ));

        let reference = CredentialRef::new(SSH_FIXTURE_KEY_REFERENCE)?;
        let authentication = SshAuthentication::PrivateKey {
            path: None,
            secret: Some(reference),
            passphrase: None,
        };
        let profile = HostConnectionProfile {
            host_id: HostId::new(),
            protocol: RemoteProtocol::Ssh,
            endpoint: config.endpoint(),
            authentication: Some(authentication),
            rdp_password: None,
            route: SshRoute {
                proxy: None,
                jump_hosts: Vec::new(),
            },
            shell_dialect: RemoteShellDialect::Posix,
            connect_timeout_ms: 20_000,
            keepalive_interval_ms: None,
        };
        let host_id = profile.host_id.clone();
        let remote = connection_service(profile, credentials, known_hosts);
        let remote: Arc<dyn RemoteConnectionService> = remote;

        let commands = Arc::new(StructuredOperationsCommandExecutor::new(Arc::clone(
            &remote,
        )));
        let tmux_observer = Arc::new(TmuxCommandObserver::new(host_id.clone()));
        let command_port: Arc<dyn OperationsCommandExecutor> = Arc::new(TmuxObservingExecutor {
            inner: commands.clone(),
            observer: tmux_observer.clone(),
        });
        let local_transport: Arc<dyn SessionTransportFactory> =
            Arc::new(LocalPtySessionTransportFactory::new());
        let sessions = Arc::new(SessionManager::new(
            local_transport,
            Handle::current(),
            SessionRuntimeConfig::default(),
        ));
        let session_port: Arc<dyn SessionService> = sessions.clone();
        let configuration_root = tempfile::tempdir()?;
        let configuration = ConfigurationStore::initialize(configuration_root.path())?;
        let profiles: Arc<dyn ProfileRepository> = Arc::new(configuration);
        let collector = Arc::new(SystemServerInfoCollector::new(
            command_port.clone(),
            Arc::clone(&remote),
        ));
        let runtime = Arc::new(SshPortForwardRuntime::new(remote.clone()));
        let operations = OperationsService::new(
            command_port,
            session_port,
            remote,
            profiles,
            collector,
            runtime,
        );

        Ok(Self {
            operations,
            commands,
            tmux_observer,
            sessions,
            host_id,
            _known_hosts_root: known_hosts_root,
            _configuration_root: configuration_root,
        })
    }
}

/// 仅记录精确 host 与任务唯一 `-L` socket 上的 tmux stdout 元数据。
struct TmuxCommandObserver {
    host_id: HostId,
    socket_name: Mutex<Option<String>>,
    format_argument: Mutex<Option<String>>,
}

impl TmuxCommandObserver {
    /// 先绑定 fixture 的 HostId，socket 在生成唯一测试 target 后才设置。
    ///
    /// @author kongweiguang
    fn new(host_id: HostId) -> Self {
        Self {
            host_id,
            socket_name: Mutex::new(None),
            format_argument: Mutex::new(None),
        }
    }

    /// 记录本次用例唯一 socket；不打印名称，避免日志扩大测试资源标识。
    fn bind_socket(&self, target: &TmuxTarget) -> Result<(), Box<dyn Error>> {
        if target.host_id.as_ref() != Some(&self.host_id) {
            return Err(
                io_error("tmux observer target does not match the isolated SSH host").into(),
            );
        }
        let socket = target
            .socket_name
            .as_ref()
            .ok_or_else(|| io_error("tmux observer requires the isolated -L socket"))?;
        let mut current = self
            .socket_name
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_some() {
            return Err(io_error("tmux observer socket was already bound").into());
        }
        *current = Some(socket.clone());
        Ok(())
    }

    /// 只匹配当前 HostId 上带精确 `-L <socket>` 的 argv，不依赖全局选项的相对顺序。
    ///
    /// @author kongweiguang
    fn matches(&self, target: &CommandExecutionTarget, invocation: &CommandInvocation) -> bool {
        let CommandExecutionTarget::Host(host_id) = target else {
            return false;
        };
        if host_id != &self.host_id || invocation.program != "tmux" {
            return false;
        }
        let socket = self
            .socket_name
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(socket) = socket.as_ref() else {
            return false;
        };
        invocation
            .arguments
            .windows(2)
            .any(|arguments| arguments[0] == "-L" && arguments[1] == *socket)
    }

    /// 仅保存任务唯一 socket 对应的固定 tmux 格式参数；该值不含主机返回数据。
    ///
    /// @author kongweiguang
    fn capture_format_argument(&self, invocation: &CommandInvocation) -> Option<String> {
        let format = invocation
            .arguments
            .windows(2)
            .find(|arguments| arguments[0] == "-F")
            .map(|arguments| arguments[1].clone());
        if let Some(format) = &format {
            *self
                .format_argument
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format.clone());
            eprintln!(
                "OPERATIONS_TMUX_FORMAT_ARGV bytes={} unit_separators={} hex={}",
                format.len(),
                format.bytes().filter(|byte| *byte == 0x1f).count(),
                bytes_hex(format.as_bytes()),
            );
        }
        format
    }

    /// 返回最近一次真实 TmuxService 调用的固定 `-F` 参数副本。
    ///
    /// @author kongweiguang
    fn captured_format_argument(&self) -> Option<String> {
        self.format_argument
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 清理命令只能使用观测器实际绑定的完整目标，不以 socket 名称前缀代替授权。
    fn authorizes_cleanup_target(&self, target: &TmuxTarget) -> bool {
        if target.host_id.as_ref() != Some(&self.host_id)
            || target.socket_path.is_some()
            || target.profile_id.is_some()
            || target.tmux_path.as_deref() != Some("tmux")
        {
            return false;
        }
        let socket = self
            .socket_name
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        socket.as_ref() == target.socket_name.as_ref()
    }

    /// 输出状态、字节结构和有限十六进制前缀；不打印 stderr 或解码任意 session/path 字段。
    fn record(&self, result: &kerminal_application::AppResult<CommandExecutionResult>) {
        const HEX_PREFIX_BYTES: usize = 96;
        match result {
            Ok(result) => {
                let field_counts = result
                    .stdout
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .map(|line| 1 + line.iter().filter(|byte| **byte == 0x1f).count())
                    .collect::<Vec<_>>();
                let mut hex = String::new();
                for byte in result.stdout.iter().take(HEX_PREFIX_BYTES) {
                    let _ = write!(&mut hex, "{byte:02x}");
                }
                let clipped = result.stdout.len() > HEX_PREFIX_BYTES;
                eprintln!(
                    "OPERATIONS_TMUX_BYTES status=exit:{:?} truncated={} stdout_len={} unit_separators={} line_feeds={} fields_per_line={field_counts:?} hex_prefix={hex} hex_clipped={clipped}",
                    result.exit_code,
                    result.truncated,
                    result.stdout.len(),
                    result.stdout.iter().filter(|byte| **byte == 0x1f).count(),
                    result.stdout.iter().filter(|byte| **byte == b'\n').count(),
                );
            }
            Err(_) => eprintln!(
                "OPERATIONS_TMUX_BYTES status=execution_error stdout_len=0 unit_separators=0 line_feeds=0 fields_per_line=[] hex_prefix= hex_clipped=false"
            ),
        }
    }
}

/// 包装真实执行器而不改写 argv、返回值或取消语义，仅为唯一 tmux socket 记录脱敏字节证据。
struct TmuxObservingExecutor {
    inner: Arc<StructuredOperationsCommandExecutor>,
    observer: Arc<TmuxCommandObserver>,
}

#[async_trait::async_trait]
impl OperationsCommandExecutor for TmuxObservingExecutor {
    /// 委托真实 executor；命令错误只记录静态结果标签，不泄露远端诊断正文。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> kerminal_application::AppResult<CommandExecutionResult> {
        let observe = self.observer.matches(&target, &invocation);
        if observe {
            self.observer.capture_format_argument(&invocation);
        }
        let result = self.inner.execute(target, invocation, limits, cancel).await;
        if observe {
            self.observer.record(&result);
        }
        result
    }

    /// 对 stdin 调用保持透明；tmux 服务不经此入口输出列表数据。
    async fn execute_with_input(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        input: Vec<u8>,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> kerminal_application::AppResult<CommandExecutionResult> {
        let observe = self.observer.matches(&target, &invocation);
        let result = self
            .inner
            .execute_with_input(target, invocation, input, limits, cancel)
            .await;
        if observe {
            self.observer.record(&result);
        }
        result
    }

    /// 透传流式调用；tmux 查询是有界缓冲命令，因此不会在这里复制或消费 stream。
    async fn execute_stream(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> kerminal_application::AppResult<OperationsCommandStream> {
        self.inner
            .execute_stream(target, invocation, limits, cancel)
            .await
    }

    /// staging 清理能力仍由原真实 executor 签发并持有。
    fn reserve_container_stage_cleanup(
        &self,
        request: ContainerStageCleanupRequest,
    ) -> kerminal_application::AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        self.inner.reserve_container_stage_cleanup(request)
    }

    /// shutdown 的 intake/cancel 语义完全交回被包装的真实 executor。
    fn begin_shutdown(&self, deadline: Instant) {
        self.inner.begin_shutdown(deadline);
    }

    /// shutdown 等待仍使用真实 executor 的在途任务和 staging owner。
    async fn shutdown_until(&self, deadline: Instant) -> kerminal_application::AppResult<()> {
        self.inner.shutdown_until(deadline).await
    }
}

/// 精确保存本次 tmux session 的候选身份，供解析失败时按孤立 socket 直接回收。
struct TmuxCleanupTarget {
    target: TmuxTarget,
    expected_names: Vec<String>,
    session_id: Option<String>,
}

/// 运行被忽略的真实 OpenSSH 验收；资源清理统一在主体结果之后执行。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private OpenSSH fixture; see this file's environment contract"]
async fn operations_server_info_forwarding_and_tmux_lifecycle() -> Result<(), Box<dyn Error>> {
    let config = OpenSshFixtureConfig::from_environment()?;
    let fixture = OperationsFixture::new(&config)?;
    let mut tmux_cleanup_target: Option<TmuxCleanupTarget> = None;
    let exercise = exercise_operations(&fixture, &config, &mut tmux_cleanup_target).await;

    let deadline = Instant::now() + Duration::from_secs(30);
    let session_cleanup = fixture.sessions.close_all_until(deadline).await;
    let tmux_cleanup = cleanup_tmux(&fixture, tmux_cleanup_target).await;
    let operations_cleanup = fixture.operations.shutdown_until(deadline).await;

    finish_acceptance(
        exercise,
        vec![
            ("sessions", session_cleanup.map_err(box_error)),
            ("tmux", tmux_cleanup),
            ("operations", operations_cleanup.map_err(box_error)),
        ],
    )
}

/// 等待 H/P 只停止任务私有 target sshd，确认活动转发异步失败、关闭数据通道并释放监听。
///
/// 该用例不控制共享 fixture；观察到 ready marker 后由外部控制脚本执行 stop，再写 signal 文件。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private OpenSSH fixture and an external stop/signal step"]
async fn operations_forwarding_reports_fixture_disconnect_and_releases_listener()
-> Result<(), Box<dyn Error>> {
    let config = OpenSshFixtureConfig::from_environment()?;
    let fixture = OperationsFixture::new(&config)?;
    let marker_directory = tempfile::tempdir()?;
    let mut forward_id = None;
    let exercise = verify_external_ssh_disconnect(
        &fixture,
        &config.target,
        marker_directory.path(),
        &mut forward_id,
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(30);
    let session_cleanup = fixture.sessions.close_all_until(deadline).await;
    let forward_cleanup = match forward_id.as_deref() {
        Some(id) if fixture.operations.port_forwards.get(id).await.is_some() => fixture
            .operations
            .port_forwards
            .stop(id)
            .await
            .map_err(box_error),
        _ => {
            eprintln!("OPERATIONS_ACCEPTANCE_CLEANUP_END=forward outcome=skipped");
            Ok(())
        }
    };
    let operations_cleanup = fixture.operations.shutdown_until(deadline).await;

    finish_acceptance(
        exercise,
        vec![
            ("sessions", session_cleanup.map_err(box_error)),
            ("forward", forward_cleanup),
            ("operations", operations_cleanup.map_err(box_error)),
        ],
    )
}

/// 建立并验证真实数据面后发布外部故障注入 marker，最终读取 Failed 状态和资源回收结果。
async fn verify_external_ssh_disconnect(
    fixture: &OperationsFixture,
    target: &SocketAddr,
    marker_directory: &std::path::Path,
    forward_id: &mut Option<String>,
) -> Result<(), Box<dyn Error>> {
    let bind = available_local_endpoint().await?;
    let snapshot = fixture
        .operations
        .port_forwards
        .start(
            forward_config(
                &fixture.host_id,
                &unique_name("fault-forward"),
                ForwardKind::Local,
                bind,
                Some(loopback_target(target.port())),
                None,
            ),
            CancellationToken::new(),
        )
        .await?;
    *forward_id = Some(snapshot.forward.id.clone());
    let address = socket_address(&snapshot.actual_bind)?;
    let mut client = TcpStream::connect(address).await?;
    verify_ssh_banner(&mut client).await?;

    let run_id = uuid::Uuid::new_v4().simple().to_string();
    tokio::fs::write(marker_directory.join("ready.txt"), &run_id).await?;
    println!(
        "OPERATIONS_SSH_FAULT_READY={} RUN_ID={run_id} FORWARD_ID={}",
        marker_directory.display(),
        snapshot.forward.id
    );
    wait_for_external_fault_signal(marker_directory, &run_id).await?;

    let failed_before = Instant::now() + Duration::from_secs(25);
    loop {
        let current = fixture
            .operations
            .port_forwards
            .get(&snapshot.forward.id)
            .await
            .ok_or_else(|| io_error("forward disappeared before failure could be observed"))?;
        if current.forward.state == ForwardState::Failed {
            if current.failure_code.is_none() {
                return Err(io_error("failed forward did not preserve an error code").into());
            }
            break;
        }
        if Instant::now() >= failed_before {
            return Err(
                io_error("SSH disconnect did not transition active forward to Failed").into(),
            );
        }
        sleep(Duration::from_millis(100)).await;
    }

    verify_stream_closed(&mut client).await?;
    wait_local_port_released(address, Instant::now() + Duration::from_secs(8)).await?;
    Ok(())
}

/// 有界等待 P/H 在停掉目标 sshd 后写入精确 run ID，拒绝错误或过期的故障信号。
async fn wait_for_external_fault_signal(
    directory: &std::path::Path,
    run_id: &str,
) -> Result<(), Box<dyn Error>> {
    let signal_path = directory.join("signal.txt");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match tokio::fs::read_to_string(&signal_path).await {
            Ok(signal) if signal.trim() == format!("stopped:{run_id}") => return Ok(()),
            Ok(_) => {
                return Err(io_error("external SSH fault signal did not match this run").into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err(
                io_error("timed out waiting for the external SSH fixture stop signal").into(),
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// 用静态阶段标签定位真实 fixture 的首个失败用例；标签不记录命令参数、路径或连接信息。
///
/// @author kongweiguang
async fn exercise_operations(
    fixture: &OperationsFixture,
    config: &OpenSshFixtureConfig,
    tmux_cleanup_target: &mut Option<TmuxCleanupTarget>,
) -> Result<(), Box<dyn Error>> {
    run_acceptance_stage(
        "python_preflight",
        ensure_remote_python(&fixture.commands, &fixture.host_id),
    )
    .await?;
    let tmux_target = tmux_target(&fixture.host_id, &unique_name("socket"));
    fixture.tmux_observer.bind_socket(&tmux_target)?;
    run_acceptance_stage("tmux_probe", async {
        let capability = fixture
            .operations
            .tmux
            .probe(tmux_target.clone(), CancellationToken::new())
            .await?;
        if !capability.available {
            return Err(io_error(
                "the isolated SSH fixture requires tmux; probe reported unavailable",
            )
            .into());
        }
        Ok(capability)
    })
    .await?;

    run_acceptance_stage(
        "server_info_snapshot",
        verify_server_info(&fixture.operations, &fixture.commands, &fixture.host_id),
    )
    .await?;
    run_acceptance_stage(
        "forward_local",
        verify_local_forward(fixture, &config.target),
    )
    .await?;
    run_acceptance_stage(
        "forward_dynamic",
        verify_local_dynamic_forward(fixture, &config.target),
    )
    .await?;
    run_acceptance_stage(
        "forward_remote",
        verify_remote_forward(fixture, &config.target),
    )
    .await?;
    run_acceptance_stage(
        "forward_remote_dynamic",
        verify_remote_dynamic_forward(fixture, &config.target),
    )
    .await?;
    run_acceptance_stage(
        "tmux_lifecycle",
        verify_tmux_lifecycle(fixture, tmux_target, tmux_cleanup_target),
    )
    .await?;
    Ok(())
}

/// 记录静态阶段的开始和结果，避免脱敏后的通用错误被误认为发生在某个调用点。
///
/// @author kongweiguang
async fn run_acceptance_stage<T, StageFuture>(
    name: &'static str,
    stage: StageFuture,
) -> Result<T, Box<dyn Error>>
where
    StageFuture: std::future::Future<Output = Result<T, Box<dyn Error>>>,
{
    eprintln!("OPERATIONS_ACCEPTANCE_STAGE_BEGIN={name}");
    match stage.await {
        Ok(value) => {
            eprintln!("OPERATIONS_ACCEPTANCE_STAGE_END={name} outcome=ok");
            Ok(value)
        }
        Err(error) => {
            eprintln!("OPERATIONS_ACCEPTANCE_STAGE_END={name} outcome=error");
            Err(error)
        }
    }
}

/// 阶段名与清理结果必须成对进入汇总，不能只保留最后一次关闭的回执。
type AcceptanceCleanup = (&'static str, Result<(), Box<dyn Error>>);

/// 保留主体错误并逐项输出 cleanup 回执，避免 `?` 提前返回掩盖后续资源关闭失败。
///
/// @author kongweiguang
fn finish_acceptance(
    exercise: Result<(), Box<dyn Error>>,
    cleanups: Vec<AcceptanceCleanup>,
) -> Result<(), Box<dyn Error>> {
    let primary = exercise.err();
    eprintln!(
        "OPERATIONS_ACCEPTANCE_EXERCISE_END=outcome={}",
        if primary.is_some() { "error" } else { "ok" }
    );
    let mut cleanup_errors = Vec::new();
    for (name, result) in cleanups {
        match result {
            Ok(()) => eprintln!("OPERATIONS_ACCEPTANCE_CLEANUP_END={name} outcome=ok"),
            Err(error) => {
                eprintln!("OPERATIONS_ACCEPTANCE_CLEANUP_END={name} outcome=error");
                cleanup_errors.push((name, error.to_string()));
            }
        }
    }
    if primary.is_none() && cleanup_errors.is_empty() {
        return Ok(());
    }
    Err(Box::new(AcceptanceFailure {
        primary,
        cleanup_errors,
    }))
}

/// 把应用层清理错误装入 acceptance 的统一汇总，而不改变运行时错误分类。
fn box_error(error: impl Error + 'static) -> Box<dyn Error> {
    Box::new(error)
}

#[derive(Debug)]
struct AcceptanceFailure {
    primary: Option<Box<dyn Error>>,
    cleanup_errors: Vec<(&'static str, String)>,
}

impl std::fmt::Display for AcceptanceFailure {
    /// 输出主体失败和每个 cleanup 失败，帮助确认错误路径仍执行了全部收尾步骤。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(primary) = &self.primary {
            write!(formatter, "exercise failed: {primary}")?;
        }
        for (name, error) in &self.cleanup_errors {
            if self.primary.is_some()
                || *name != self.cleanup_errors.first().map_or("", |item| item.0)
            {
                write!(formatter, "; ")?;
            }
            write!(formatter, "cleanup {name} failed: {error}")?;
        }
        Ok(())
    }
}

impl Error for AcceptanceFailure {
    /// 主体错误仍可由测试 runner 的 error chain 检查；次级 cleanup 错误保存在 Display 中。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.primary.as_deref()
    }
}

/// 对照同一 SSH 目标的独立 uname 输出验证身份字段，避免 architecture/release 顺序回归。
///
/// @author kongweiguang
async fn verify_server_info(
    operations: &OperationsService,
    commands: &StructuredOperationsCommandExecutor,
    host_id: &HostId,
) -> Result<(), Box<dyn Error>> {
    let snapshot = operations
        .server_info
        .snapshot(
            ServerInfoQuery {
                target: ServerInfoTarget::Ssh {
                    host_id: host_id.clone(),
                },
                tier: ServerInfoTier::Full,
            },
            CancellationToken::new(),
        )
        .await?;
    if snapshot.hostname.as_deref().is_none_or(str::is_empty)
        || snapshot
            .operating_system
            .as_deref()
            .is_none_or(str::is_empty)
        || snapshot.memory.total_bytes.is_none_or(|total| total == 0)
    {
        return Err(
            io_error("read-only server-info snapshot omitted Linux identity or memory").into(),
        );
    }

    let architecture = run_remote(commands, host_id, "uname", vec!["-m".to_owned()]).await?;
    ensure_remote_success(&architecture, "Linux architecture")?;
    let architecture = std::str::from_utf8(&architecture.stdout)?.trim();
    let kernel = run_remote(commands, host_id, "uname", vec!["-r".to_owned()]).await?;
    ensure_remote_success(&kernel, "Linux kernel release")?;
    let kernel = std::str::from_utf8(&kernel.stdout)?.trim();
    if snapshot.architecture.as_deref() != Some(architecture)
        || snapshot.kernel.as_deref() != Some(kernel)
    {
        return Err(io_error("server-info architecture/kernel fields differed from uname").into());
    }
    Ok(())
}

/// 检查任务 sshd 可用 Python 3 作为安全的结构化远端探针和远端转发客户端。
async fn ensure_remote_python(
    commands: &StructuredOperationsCommandExecutor,
    host_id: &HostId,
) -> Result<(), Box<dyn Error>> {
    let result = run_remote(
        commands,
        host_id,
        "python3",
        vec![
            "-c".to_owned(),
            "import sys; print(sys.version_info[0])".to_owned(),
        ],
    )
    .await?;
    if result.exit_code != Some(0) {
        return Err(io_error("the isolated Linux SSH fixture requires python3").into());
    }
    Ok(())
}

/// 通过真实 TCP payload 验证 local forward，并在停止后证明本机监听端口已释放。
async fn verify_local_forward(
    fixture: &OperationsFixture,
    target: &SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let bind = available_local_endpoint().await?;
    let snapshot = fixture
        .operations
        .port_forwards
        .start(
            forward_config(
                &fixture.host_id,
                &unique_name("local-forward"),
                ForwardKind::Local,
                bind,
                Some(loopback_target(target.port())),
                None,
            ),
            CancellationToken::new(),
        )
        .await?;
    let address = socket_address(&snapshot.actual_bind)?;
    let mut client = TcpStream::connect(address).await?;
    verify_ssh_banner(&mut client).await?;
    fixture
        .operations
        .port_forwards
        .stop(&snapshot.forward.id)
        .await?;
    verify_stream_closed(&mut client).await?;
    verify_local_port_released(address).await?;
    Ok(())
}

/// 使用成熟 SOCKS5 客户端经 local dynamic forward 到达任务 sshd，再验证关闭监听。
async fn verify_local_dynamic_forward(
    fixture: &OperationsFixture,
    target: &SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let bind = available_local_endpoint().await?;
    let snapshot = fixture
        .operations
        .port_forwards
        .start(
            forward_config(
                &fixture.host_id,
                &unique_name("dynamic-forward"),
                ForwardKind::Dynamic,
                bind,
                None,
                Some(ProxyProtocol::Socks5),
            ),
            CancellationToken::new(),
        )
        .await?;
    let proxy_address = socket_address(&snapshot.actual_bind)?;
    if snapshot.proxy_url.is_none() {
        return Err(io_error("dynamic forward did not expose its actual proxy URL").into());
    }
    let stream = TcpStream::connect(proxy_address).await?;
    let destination_host = Ipv4Addr::LOCALHOST.to_string();
    let mut socks =
        Socks5Stream::connect_with_socket(stream, (destination_host.as_str(), target.port()))
            .await?;
    verify_ssh_banner(&mut socks).await?;
    fixture
        .operations
        .port_forwards
        .stop(&snapshot.forward.id)
        .await?;
    verify_stream_closed(&mut socks).await?;
    verify_local_port_released(proxy_address).await?;
    Ok(())
}

/// 使用远端回环客户端验证 remote forward 的 TCP payload 和服务端监听器撤销。
async fn verify_remote_forward(
    fixture: &OperationsFixture,
    target: &SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let remote_port = remote_ephemeral_port(&fixture.commands, &fixture.host_id).await?;
    let bind = ForwardEndpoint::new("127.0.0.1", remote_port)
        .map_err(|_| io_error("SSH fixture returned an invalid remote-forward port"))?;
    let snapshot = fixture
        .operations
        .port_forwards
        .start(
            forward_config(
                &fixture.host_id,
                &unique_name("remote-forward"),
                ForwardKind::Remote,
                bind,
                Some(loopback_target(target.port())),
                None,
            ),
            CancellationToken::new(),
        )
        .await?;
    let actual = snapshot.actual_bind.clone();
    let result = run_remote(
        &fixture.commands,
        &fixture.host_id,
        "python3",
        vec![
            "-c".to_owned(),
            REMOTE_READ_BANNER.to_owned(),
            actual.host.clone(),
            actual.port.get().to_string(),
        ],
    )
    .await?;
    ensure_ssh_banner(&result.stdout)?;
    fixture
        .operations
        .port_forwards
        .stop(&snapshot.forward.id)
        .await?;
    verify_remote_port_released(&fixture.commands, &fixture.host_id, &actual).await?;
    Ok(())
}

/// 从远端 SOCKS5 客户端访问任务 sshd，覆盖 remote dynamic 的 accept 与双向数据泵。
async fn verify_remote_dynamic_forward(
    fixture: &OperationsFixture,
    target: &SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let snapshot = run_acceptance_stage("forward_remote_dynamic_start", async {
        let remote_port = remote_ephemeral_port(&fixture.commands, &fixture.host_id).await?;
        let bind = ForwardEndpoint::new("127.0.0.1", remote_port)
            .map_err(|_| io_error("SSH fixture returned an invalid remote-dynamic port"))?;
        fixture
            .operations
            .port_forwards
            .start(
                forward_config(
                    &fixture.host_id,
                    &unique_name("remote-dynamic"),
                    ForwardKind::RemoteDynamic,
                    bind,
                    None,
                    Some(ProxyProtocol::Socks5),
                ),
                CancellationToken::new(),
            )
            .await
            .map_err(box_error)
    })
    .await?;
    let actual = snapshot.actual_bind.clone();
    let result = run_acceptance_stage("forward_remote_dynamic_data", async {
        let result = run_remote(
            &fixture.commands,
            &fixture.host_id,
            "python3",
            vec![
                "-c".to_owned(),
                REMOTE_SOCKS_READ_BANNER.to_owned(),
                actual.host.clone(),
                actual.port.get().to_string(),
                target.port().to_string(),
            ],
        )
        .await?;
        ensure_remote_success(&result, "remote dynamic SOCKS data")?;
        Ok(result)
    })
    .await?;
    run_acceptance_stage("forward_remote_dynamic_banner", async {
        ensure_ssh_banner(&result.stdout)
    })
    .await?;
    run_acceptance_stage("forward_remote_dynamic_stop", async {
        fixture
            .operations
            .port_forwards
            .stop(&snapshot.forward.id)
            .await
            .map_err(box_error)
    })
    .await?;
    run_acceptance_stage("forward_remote_dynamic_release", async {
        verify_remote_port_released(&fixture.commands, &fixture.host_id, &actual).await
    })
    .await?;
    Ok(())
}

/// 为每个实际 tmux 用例调用记录静态子阶段，并保留创建回执供解析失败后的精确清理。
/// 诊断只在已捕获 format 且原创建失败时运行，诊断失败不能替换原始错误。
///
/// @author kongweiguang
async fn verify_tmux_lifecycle(
    fixture: &OperationsFixture,
    target: TmuxTarget,
    tmux_cleanup_target: &mut Option<TmuxCleanupTarget>,
) -> Result<(), Box<dyn Error>> {
    let session_name = unique_name("session");
    let renamed_name = unique_name("session-renamed");
    *tmux_cleanup_target = Some(TmuxCleanupTarget {
        target: target.clone(),
        expected_names: vec![session_name.clone(), renamed_name.clone()],
        session_id: None,
    });
    let create_result = run_acceptance_stage("tmux_create", async {
        fixture
            .operations
            .tmux
            .create_session(
                TmuxCreateRequest {
                    target: target.clone(),
                    name: session_name.clone(),
                    working_directory: Some("/tmp".to_owned()),
                },
                CancellationToken::new(),
            )
            .await
            .map_err(box_error)
    })
    .await;
    if let Err(primary) = create_result {
        if let Some(format) = fixture.tmux_observer.captured_format_argument()
            && run_acceptance_stage(
                "tmux_format_transport_diagnostic",
                verify_tmux_format_transport(fixture, &target, &format),
            )
            .await
            .is_err()
        {
            eprintln!("OPERATIONS_TMUX_FORMAT_DIAGNOSTIC_END=error");
        }
        return Err(primary);
    }
    let created = create_result.expect("create result was checked above");

    let format = fixture
        .tmux_observer
        .captured_format_argument()
        .ok_or_else(|| io_error("tmux create did not expose its fixed -F format"))?;
    run_acceptance_stage(
        "tmux_format_transport",
        verify_tmux_format_transport(fixture, &target, &format),
    )
    .await?;
    if let Some(cleanup) = tmux_cleanup_target.as_mut() {
        cleanup.session_id = Some(created.session.id.clone());
    }

    run_acceptance_stage("tmux_list_after_create", async {
        let sessions = fixture
            .operations
            .tmux
            .list_sessions(target.clone(), CancellationToken::new())
            .await
            .map_err(box_error)?;
        if !sessions
            .iter()
            .any(|session| session.id == created.session.id && session.name == session_name)
        {
            return Err(io_error("new tmux session was absent from its isolated socket").into());
        }
        Ok(())
    })
    .await?;

    let rename_receipt = run_acceptance_stage("tmux_rename", async {
        fixture
            .operations
            .tmux
            .rename_session(
                target.clone(),
                created.session.id.clone(),
                renamed_name.clone(),
                CancellationToken::new(),
            )
            .await
            .map_err(box_error)
    })
    .await?;
    if rename_receipt.session.name != renamed_name {
        return Err(io_error("tmux rename receipt did not return the new session name").into());
    }
    run_acceptance_stage("tmux_list_after_rename", async {
        let sessions = fixture
            .operations
            .tmux
            .list_sessions(target.clone(), CancellationToken::new())
            .await
            .map_err(box_error)?;
        if !sessions
            .iter()
            .any(|session| session.id == created.session.id && session.name == renamed_name)
        {
            return Err(
                io_error("tmux rename was not visible in the refreshed session list").into(),
            );
        }
        Ok(())
    })
    .await?;

    run_acceptance_stage(
        "tmux_attach_cancel",
        verify_cancelled_tmux_attach(fixture, &target, &created.session.id),
    )
    .await?;

    let attached = run_acceptance_stage("tmux_attach", async {
        fixture
            .operations
            .tmux
            .attach_session(
                target.clone(),
                created.session.id.clone(),
                SessionOwner::Service {
                    name: "operations-ssh-acceptance".to_owned(),
                },
                TerminalSize::default(),
                CancellationToken::new(),
            )
            .await
            .map_err(box_error)
    })
    .await?;

    run_acceptance_stage("tmux_list_after_attach", async {
        let attach_deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let sessions = fixture
                .operations
                .tmux
                .list_sessions(target.clone(), CancellationToken::new())
                .await
                .map_err(box_error)?;
            if sessions
                .iter()
                .any(|session| session.id == created.session.id && session.attached)
            {
                return Ok(());
            }
            if Instant::now() >= attach_deadline {
                return Err(io_error("tmux did not report the Kerminal attach client").into());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await?;

    run_acceptance_stage("tmux_detach", async {
        fixture
            .sessions
            .close_until(
                attached.session_id,
                Instant::now() + Duration::from_secs(12),
            )
            .await
            .map_err(box_error)
    })
    .await?;

    run_acceptance_stage("tmux_list_after_detach", async {
        let detach_deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let remaining = fixture
                .operations
                .tmux
                .list_sessions(target.clone(), CancellationToken::new())
                .await
                .map_err(box_error)?;
            match remaining
                .iter()
                .find(|session| session.id == created.session.id)
            {
                Some(session) if !session.attached => return Ok(()),
                None => {
                    return Err(io_error(
                        "tmux session disappeared when its Kerminal attach closed",
                    )
                    .into());
                }
                _ if Instant::now() < detach_deadline => sleep(Duration::from_millis(100)).await,
                _ => {
                    return Err(
                        io_error("closing the Kerminal attach did not detach tmux client").into(),
                    );
                }
            }
        }
    })
    .await?;

    run_acceptance_stage("tmux_kill", async {
        let killed = fixture
            .operations
            .tmux
            .kill_session(
                target.clone(),
                created.session.id.clone(),
                CancellationToken::new(),
            )
            .await
            .map_err(box_error)?;
        if !killed {
            return Err(io_error("tmux kill did not return a successful deletion receipt").into());
        }
        Ok(())
    })
    .await?;
    run_acceptance_stage("tmux_list_after_kill", async {
        let remaining = fixture
            .operations
            .tmux
            .list_sessions(target, CancellationToken::new())
            .await
            .map_err(box_error)?;
        if remaining
            .iter()
            .any(|session| session.id == created.session.id)
        {
            return Err(io_error(
                "tmux kill-session returned before the target session disappeared",
            )
            .into());
        }
        Ok(())
    })
    .await?;
    *tmux_cleanup_target = None;
    Ok(())
}

/// 通过同一 SSH executor 对照 argv 接收、默认非 UTF-8 输出与显式 `-u` 输出。
///
/// @author kongweiguang
async fn verify_tmux_format_transport(
    fixture: &OperationsFixture,
    target: &TmuxTarget,
    format: &str,
) -> Result<(), Box<dyn Error>> {
    let host_id = target
        .host_id
        .clone()
        .ok_or_else(|| io_error("tmux format probe requires the isolated SSH host"))?;
    let expected_hex = bytes_hex(format.as_bytes());
    let mut python = CommandInvocation::new(
        "python3",
        vec![
            "-c".to_owned(),
            "import sys;sys.stdout.write(sys.argv[1].encode().hex())".to_owned(),
            format.to_owned(),
        ],
    );
    python
        .environment
        .insert("LC_ALL".to_owned(), "C".to_owned());
    let probe = fixture
        .commands
        .execute(
            CommandExecutionTarget::Host(host_id),
            python,
            CommandExecutionLimits {
                timeout: OPERATIONS_COMMAND_TIMEOUT,
                max_output_bytes: MAX_REMOTE_OUTPUT_BYTES,
            },
            CancellationToken::new(),
        )
        .await
        .map_err(box_error)?;
    if probe.exit_code != Some(0) || probe.truncated || probe.stdout != expected_hex.as_bytes() {
        return Err(
            io_error("remote shell did not preserve the synthetic tmux format argv").into(),
        );
    }

    let plain = run_tmux_format_list(fixture, target, format, false).await?;
    let utf8 = run_tmux_format_list(fixture, target, format, true).await?;
    if plain.exit_code != Some(0) || utf8.exit_code != Some(0) || plain.truncated || utf8.truncated
    {
        return Err(io_error("tmux format output mode probe did not complete").into());
    }
    let expected_separators = format.bytes().filter(|byte| *byte == 0x1f).count();
    let plain_separators = plain.stdout.iter().filter(|byte| **byte == 0x1f).count();
    let utf8_separators = utf8.stdout.iter().filter(|byte| **byte == 0x1f).count();
    eprintln!(
        "OPERATIONS_TMUX_FORMAT_MODES plain_len={} plain_us={} plain_underscores={} plain_hex_prefix={} utf8_len={} utf8_us={} utf8_hex_prefix={}",
        plain.stdout.len(),
        plain_separators,
        plain.stdout.iter().filter(|byte| **byte == b'_').count(),
        bytes_hex(&plain.stdout[..plain.stdout.len().min(96)]),
        utf8.stdout.len(),
        utf8_separators,
        bytes_hex(&utf8.stdout[..utf8.stdout.len().min(96)]),
    );
    if plain_separators != 0 || utf8_separators != expected_separators {
        return Err(io_error("tmux UTF-8 output flag did not preserve format separators").into());
    }
    Ok(())
}

/// 在同一隔离 socket 上查询 session，仅切换 tmux 客户端的 UTF-8 输出标志。
///
/// @author kongweiguang
async fn run_tmux_format_list(
    fixture: &OperationsFixture,
    target: &TmuxTarget,
    format: &str,
    force_utf8: bool,
) -> Result<CommandExecutionResult, Box<dyn Error>> {
    let host_id = target
        .host_id
        .clone()
        .ok_or_else(|| io_error("tmux format probe requires the isolated SSH host"))?;
    let socket = target
        .socket_name
        .as_ref()
        .ok_or_else(|| io_error("tmux format probe requires the isolated -L socket"))?;
    let mut arguments = Vec::new();
    if force_utf8 {
        arguments.push("-u".to_owned());
    }
    arguments.extend([
        "-L".to_owned(),
        socket.clone(),
        "list-sessions".to_owned(),
        "-F".to_owned(),
        format.to_owned(),
    ]);
    let mut invocation = CommandInvocation::new(
        target
            .tmux_path
            .clone()
            .unwrap_or_else(|| "tmux".to_owned()),
        arguments,
    );
    invocation
        .environment
        .insert("LC_ALL".to_owned(), "C".to_owned());
    fixture
        .commands
        .execute(
            CommandExecutionTarget::Host(host_id),
            invocation,
            CommandExecutionLimits {
                timeout: OPERATIONS_COMMAND_TIMEOUT,
                max_output_bytes: MAX_REMOTE_OUTPUT_BYTES,
            },
            CancellationToken::new(),
        )
        .await
        .map_err(box_error)
}

/// 将只含固定 tmux 格式模板或合成验收数据的字节转为小写十六进制。
///
/// @author kongweiguang
fn bytes_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

/// 取消真实 SSH attach 尝试后回读 tmux client 状态，确保取消没有留下未发布的 attach。
async fn verify_cancelled_tmux_attach(
    fixture: &OperationsFixture,
    target: &TmuxTarget,
    session_id: &str,
) -> Result<(), Box<dyn Error>> {
    let cancellation = CancellationToken::new();
    let cancel_after = cancellation.clone();
    let tmux = fixture.operations.tmux.clone();
    let target_for_attach = target.clone();
    let session_id_for_attach = session_id.to_owned();
    let attach = tokio::spawn(async move {
        tmux.attach_session(
            target_for_attach,
            session_id_for_attach,
            SessionOwner::Service {
                name: "operations-ssh-cancel-acceptance".to_owned(),
            },
            TerminalSize::default(),
            cancellation,
        )
        .await
    });
    sleep(Duration::from_millis(30)).await;
    cancel_after.cancel();
    let result = tokio::time::timeout(Duration::from_secs(12), attach)
        .await
        .map_err(|_| io_error("cancelled tmux attach did not return before the deadline"))??;
    match result {
        Ok(session) => {
            fixture
                .sessions
                .close_until(session.session_id, Instant::now() + Duration::from_secs(12))
                .await?;
        }
        Err(error) if error.public_code() == kerminal_application::PublicErrorCode::Cancelled => {}
        Err(error) => return Err(error.into()),
    }

    let detached_by = Instant::now() + Duration::from_secs(8);
    loop {
        let sessions = fixture
            .operations
            .tmux
            .list_sessions(target.clone(), CancellationToken::new())
            .await?;
        match sessions.iter().find(|session| session.id == session_id) {
            Some(session) if !session.attached => return Ok(()),
            None => {
                return Err(io_error("tmux session disappeared during attach cancellation").into());
            }
            _ if Instant::now() < detached_by => sleep(Duration::from_millis(100)).await,
            _ => return Err(io_error("cancelled tmux attach left a persistent client").into()),
        }
    }
}

/// 不调用易失败的 session parser，按已知 ID 与合成名称回收并验证唯一测试 socket。
async fn cleanup_tmux(
    fixture: &OperationsFixture,
    cleanup_target: Option<TmuxCleanupTarget>,
) -> Result<(), Box<dyn Error>> {
    let Some(cleanup_target) = cleanup_target else {
        return Ok(());
    };
    let mut identities = Vec::new();
    if let Some(session_id) = cleanup_target.session_id {
        identities.push(session_id);
    }
    for name in cleanup_target.expected_names {
        if !identities.contains(&name) {
            identities.push(name);
        }
    }

    let mut failures = Vec::new();
    for (index, identity) in identities.iter().enumerate() {
        let exists = match tmux_session_exists(fixture, &cleanup_target.target, identity).await {
            Ok(exists) => exists,
            Err(error) => {
                failures.push(format!("identity_check[{index}]: {error}"));
                continue;
            }
        };
        if !exists {
            continue;
        }

        let kill_result = tmux_cli(
            fixture,
            &cleanup_target.target,
            vec!["kill-session".to_owned(), "-t".to_owned(), identity.clone()],
        )
        .await;
        let still_exists = tmux_session_exists(fixture, &cleanup_target.target, identity).await;
        match still_exists {
            Ok(false) => {
                if let Err(error) = kill_result {
                    eprintln!(
                        "OPERATIONS_ACCEPTANCE_CLEANUP_END=tmux_identity[{index}] outcome=removed_after_error public_code={:?}",
                        error.public_code()
                    );
                }
            }
            Ok(true) => {
                let detail = match kill_result {
                    Ok(result) => format!("kill_exit={:?}", result.exit_code),
                    Err(error) => format!("kill_error={}", error),
                };
                failures.push(format!(
                    "identity_check[{index}]: identity remains ({detail})"
                ));
            }
            Err(error) => failures.push(format!("readback[{index}]: {error}")),
        }
    }
    if !failures.is_empty() {
        return Err(io_error(&format!(
            "tmux isolated-socket cleanup left {} unresolved action(s): {}",
            failures.len(),
            failures.join("; ")
        ))
        .into());
    }
    Ok(())
}

/// 直接执行结构化 argv，要求目标与本 fixture 捕获的 host/socket 完全一致。
async fn tmux_cli(
    fixture: &OperationsFixture,
    target: &TmuxTarget,
    arguments: Vec<String>,
) -> Result<CommandExecutionResult, kerminal_application::AppError> {
    let socket = target.socket_name.as_ref().ok_or_else(|| {
        kerminal_application::AppError::invalid("tmux cleanup requires -L socket")
    })?;
    let host_id = target.host_id.clone().ok_or_else(|| {
        kerminal_application::AppError::invalid("tmux cleanup requires SSH target")
    })?;
    if !fixture.tmux_observer.authorizes_cleanup_target(target) {
        return Err(kerminal_application::AppError::invalid(
            "tmux cleanup target does not match the captured fixture socket",
        ));
    }
    let mut scoped_arguments = vec!["-L".to_owned(), socket.clone()];
    scoped_arguments.extend(arguments);
    let mut invocation = CommandInvocation::new(
        target
            .tmux_path
            .clone()
            .unwrap_or_else(|| "tmux".to_owned()),
        scoped_arguments,
    );
    invocation
        .environment
        .insert("LC_ALL".to_owned(), "C".to_owned());
    fixture
        .commands
        .execute(
            CommandExecutionTarget::Host(host_id),
            invocation,
            CommandExecutionLimits {
                timeout: OPERATIONS_COMMAND_TIMEOUT,
                max_output_bytes: MAX_REMOTE_OUTPUT_BYTES,
            },
            CancellationToken::new(),
        )
        .await
}

/// 仅接受 tmux 明确报告的 session 存在或不存在；静默的非零结果保持为清理失败。
async fn tmux_session_exists(
    fixture: &OperationsFixture,
    target: &TmuxTarget,
    identity: &str,
) -> Result<bool, Box<dyn Error>> {
    let result = tmux_cli(
        fixture,
        target,
        vec![
            "has-session".to_owned(),
            "-t".to_owned(),
            identity.to_owned(),
        ],
    )
    .await
    .map_err(box_error)?;
    if result.truncated || !result.stdout.is_empty() {
        return Err(io_error("tmux has-session returned unexpected output").into());
    }
    match result.exit_code {
        Some(0) if result.stderr.is_empty() => Ok(true),
        Some(1)
            if is_known_missing_tmux_session(&result.stderr, identity)
                || target
                    .socket_name
                    .as_deref()
                    .is_some_and(|socket| is_known_no_tmux_server(&result.stderr, socket)) =>
        {
            Ok(false)
        }
        Some(0 | 1) => Err(io_error("tmux has-session returned an unrecognized diagnostic").into()),
        Some(code) => Err(io_error(&format!(
            "tmux has-session returned unexpected exit status {code}"
        ))
        .into()),
        None => Err(io_error("tmux has-session did not return an exit status").into()),
    }
}

/// 只把 tmux 在 C locale 下精确指向本次 socket 的“server 不存在”诊断当作无 session。
fn is_known_no_tmux_server(stderr: &[u8], socket_name: &str) -> bool {
    let line = stderr.strip_suffix(b"\n").unwrap_or(stderr);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(socket_path) = line.strip_prefix(b"no server running on ") else {
        return false;
    };
    let socket_bytes = socket_name.as_bytes();
    socket_path.starts_with(b"/")
        && socket_path.ends_with(socket_bytes)
        && socket_path.get(socket_path.len().saturating_sub(socket_bytes.len() + 1)) == Some(&b'/')
        && !socket_path.contains(&b'\n')
        && !socket_path.contains(&b'\r')
}

/// 只接受和本次 `-t` 参数完全相同的 tmux 缺失 session 诊断。
fn is_known_missing_tmux_session(stderr: &[u8], identity: &str) -> bool {
    let line = stderr.strip_suffix(b"\n").unwrap_or(stderr);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut expected = b"can't find session: ".to_vec();
    expected.extend_from_slice(identity.as_bytes());
    line == expected.as_slice()
}

/// 通过业务命令端口选取远端短暂空闲的 loopback 端口，不创建或修改目标服务。
async fn remote_ephemeral_port(
    commands: &StructuredOperationsCommandExecutor,
    host_id: &HostId,
) -> Result<u16, Box<dyn Error>> {
    let result = run_remote(
        commands,
        host_id,
        "python3",
        vec![
            "-c".to_owned(),
            "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()".to_owned(),
        ],
    )
    .await?;
    ensure_remote_success(&result, "remote ephemeral port allocation")?;
    Ok(String::from_utf8(result.stdout)?.trim().parse()?)
}

/// 停止 remote forward 后，由远端探针确认 listener 已撤销而不是只更改 UI 状态。
async fn verify_remote_port_released(
    commands: &StructuredOperationsCommandExecutor,
    host_id: &HostId,
    endpoint: &ForwardEndpoint,
) -> Result<(), Box<dyn Error>> {
    let result = run_remote(
        commands,
        host_id,
        "python3",
        vec![
            "-c".to_owned(),
            REMOTE_ASSERT_CLOSED.to_owned(),
            endpoint.host.clone(),
            endpoint.port.get().to_string(),
        ],
    )
    .await?;
    ensure_remote_success(&result, "remote listener release")
}

/// 检查目标 SSH 服务的只读 banner，确认转发传输了真实网络数据。
async fn verify_ssh_banner<R>(reader: &mut R) -> Result<(), Box<dyn Error>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let banner = tokio::time::timeout(Duration::from_secs(6), read_ssh_banner(reader)).await??;
    ensure_ssh_banner(&banner)
}

/// 在关闭转发后读取已接受通道，确认 stop 回收数据面而非只释放监听端口。
async fn verify_stream_closed<R>(reader: &mut R) -> Result<(), Box<dyn Error>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut byte = [0_u8; 1];
    match tokio::time::timeout(Duration::from_secs(6), reader.read(&mut byte)).await {
        Ok(Ok(0)) => Ok(()),
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(())
        }
        Ok(Ok(_)) => Err(io_error("stopped tunnel delivered unexpected post-stop payload").into()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(io_error("accepted tunnel stream stayed open after stop").into()),
    }
}

/// 分行读取 SSH identification，兼容 banner 前有服务说明行的 SSH 实现。
async fn read_ssh_banner<R>(reader: &mut R) -> Result<Vec<u8>, Box<dyn Error>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut byte = [0_u8; 1];
    for _ in 0..1024 {
        let count = reader.read(&mut byte).await?;
        if count == 0 {
            return Err(io_error("SSH tunnel closed before identification was received").into());
        }
        output.push(byte[0]);
        if byte[0] == b'\n' && output.windows(7).any(|window| window == b"SSH-2.0") {
            return Ok(output);
        }
    }
    Err(io_error("SSH identification exceeded the acceptance bound").into())
}

/// 断言 payload 中确实含 SSH-2 identification，不将 SOCKS 握手成功当成 tunnel 成功。
fn ensure_ssh_banner(bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if bytes.windows(7).any(|window| window == b"SSH-2.0") {
        Ok(())
    } else {
        Err(io_error("forwarded payload did not contain an SSH-2.0 identification").into())
    }
}

/// 用明确 HostId 执行远端测试探针；测试参数始终作为独立 argv 项传递。
async fn run_remote(
    commands: &StructuredOperationsCommandExecutor,
    host_id: &HostId,
    program: &str,
    arguments: Vec<String>,
) -> Result<kerminal_application::operations::CommandExecutionResult, Box<dyn Error>> {
    Ok(commands
        .execute(
            CommandExecutionTarget::Host(host_id.clone()),
            CommandInvocation::new(program, arguments),
            CommandExecutionLimits {
                timeout: OPERATIONS_COMMAND_TIMEOUT,
                max_output_bytes: MAX_REMOTE_OUTPUT_BYTES,
            },
            CancellationToken::new(),
        )
        .await?)
}

/// 检查远端探针退出码，错误文案不包含原始命令输出或连接配置。
fn ensure_remote_success(
    result: &kerminal_application::operations::CommandExecutionResult,
    purpose: &str,
) -> Result<(), Box<dyn Error>> {
    if result.exit_code == Some(0) && !result.truncated {
        Ok(())
    } else {
        Err(io_error(&format!("remote acceptance probe failed: {purpose}")).into())
    }
}

/// 请求本机 IPv4 ephemeral port 并立即释放，供仅允许非零端口的领域配置使用。
async fn available_local_endpoint() -> Result<ForwardEndpoint, Box<dyn Error>> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let endpoint = ForwardEndpoint::new(Ipv4Addr::LOCALHOST.to_string(), port)
        .map_err(|_| io_error("the allocated local test port is invalid"))?;
    Ok(endpoint)
}

/// 将领域 endpoint 校验为本机 socket 地址，避免测试客户端再次猜测 bind host。
fn socket_address(endpoint: &ForwardEndpoint) -> Result<SocketAddr, Box<dyn Error>> {
    Ok(SocketAddr::new(
        endpoint.host.parse::<IpAddr>()?,
        endpoint.port.get(),
    ))
}

/// 在停止后重新绑定同一监听地址，验证本机 socket 和已接受数据通道都已回收。
async fn verify_local_port_released(address: SocketAddr) -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind(address).await?;
    drop(listener);
    Ok(())
}

/// 端口转发故障 watcher 先发布 Failed 再异步关闭 listener，因此给实际回收留出有界窗口。
async fn wait_local_port_released(
    address: SocketAddr,
    deadline: Instant,
) -> Result<(), Box<dyn Error>> {
    loop {
        match TcpListener::bind(address).await {
            Ok(listener) => {
                drop(listener);
                return Ok(());
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// 构造仅指向任务 sshd loopback 的转发目标，避免端到端测试访问其它网络地址。
fn loopback_target(port: u16) -> ForwardEndpoint {
    ForwardEndpoint::new(Ipv4Addr::LOCALHOST.to_string(), port)
        .expect("the configured SSH fixture has a nonzero port")
}

/// 每项转发使用新 ID 与 socket 地址，所有目标仍绑定同一显式 SSH host。
fn forward_config(
    host_id: &HostId,
    name: &str,
    kind: ForwardKind,
    bind: ForwardEndpoint,
    destination: Option<ForwardEndpoint>,
    proxy_protocol: Option<ProxyProtocol>,
) -> PortForwardConfig {
    let remote = matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic);
    PortForwardConfig {
        host_id: host_id.clone(),
        name: name.to_owned(),
        kind,
        bind,
        destination,
        proxy_protocol,
        remote_access_scope: remote.then_some(RemoteAccessScope::Loopback),
        proxy_apply_scope: ProxyApplyScope::None,
    }
}

/// 给每个测试资源生成不共享的名称，避免清理时命中其他任务或用户服务。
fn unique_name(prefix: &str) -> String {
    format!("kerminal-gpui-{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// 使用唯一 `-L` socket 绑定 SSH 主机，隔离 tmux server 与用户默认 socket。
fn tmux_target(host_id: &HostId, socket_name: &str) -> TmuxTarget {
    TmuxTarget {
        host_id: Some(host_id.clone()),
        profile_id: None,
        socket_name: Some(socket_name.to_owned()),
        socket_path: None,
        tmux_path: Some("tmux".to_owned()),
    }
}

const REMOTE_READ_BANNER: &str = r#"import socket,sys
s=socket.create_connection((sys.argv[1],int(sys.argv[2])),timeout=6)
s.settimeout(6)
data=b''
while b'\n' not in data and len(data)<1024:
 part=s.recv(1)
 if not part: raise RuntimeError('SSH banner stream closed')
 data+=part
sys.stdout.buffer.write(data)
s.close()
"#;

const REMOTE_SOCKS_READ_BANNER: &str = r#"import socket,struct,sys
def exact(s,n):
 data=b''
 while len(data)<n:
  part=s.recv(n-len(data))
  if not part: raise RuntimeError('SOCKS stream closed')
  data+=part
 return data
s=socket.create_connection((sys.argv[1],int(sys.argv[2])),timeout=6)
s.settimeout(6)
s.sendall(b'\x05\x01\x00')
if exact(s,2)!=b'\x05\x00': raise RuntimeError('SOCKS5 no-auth rejected')
s.sendall(b'\x05\x01\x00\x01'+socket.inet_aton('127.0.0.1')+struct.pack('!H',int(sys.argv[3])))
head=exact(s,4)
if head[1]!=0: raise RuntimeError('SOCKS5 CONNECT failed')
if head[3]==1: exact(s,6)
elif head[3]==4: exact(s,18)
elif head[3]==3: exact(s,exact(s,1)[0]+2)
else: raise RuntimeError('SOCKS5 address type invalid')
data=b''
while b'\n' not in data and len(data)<1024:
 part=s.recv(1)
 if not part: raise RuntimeError('SSH banner stream closed')
 data+=part
sys.stdout.buffer.write(data)
s.close()
"#;

const REMOTE_ASSERT_CLOSED: &str = r#"import socket,sys,time
for _ in range(30):
 s=socket.socket(); s.settimeout(.2); code=s.connect_ex((sys.argv[1],int(sys.argv[2]))); s.close()
 if code!=0: sys.exit(0)
 time.sleep(.1)
sys.exit(2)
"#;
