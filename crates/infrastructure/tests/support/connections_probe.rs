//! 未保存连接探测 adapter 的外置单元测试；不进行网络或设备访问。
//!
//! @author kongweiguang

use super::*;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use kerminal_application::{
    AppError,
    connections::{
        CommandStreamHandle, ConnectionIntent, ExecRequest, HostKeyChallenge,
        HostKeyRotationReceipt, HostKeyRotationRequest, HostKeyTrustReceipt, HostKeyTrustRequest,
        OpenSftpRequest, RemoteConnectionHandle, RemoteConnectionLease, RemoteConnectionService,
        RemoteForwardHandle, RemoteForwardRequest, SftpChannelHandle, ShellScriptRequest,
        TerminalOpenRequest, TunnelChannelHandle, TunnelRequest,
    },
    sessions::{
        SessionOpenCleanupOwner, SessionTransport, SessionTransportControl,
        SessionTransportFactory, SessionTransportOpenError,
    },
};
use kerminal_domain::{
    connections::{ConnectionCapabilities, RemoteShellDialect, SshRoute},
    files::RemotePath,
    identity::HostId,
    sessions::{SerialSessionTarget, SessionExitStatus, SessionTarget, TerminalSize},
};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;

struct FixedKnownHosts {
    status: HostKeyStatus,
    trust_calls: AtomicUsize,
}

#[async_trait]
impl KnownHostsRepository for FixedKnownHosts {
    /// 测试仓储返回固定状态，避免用真实用户 known_hosts 或访问网络。
    async fn inspect(
        &self,
        _endpoint: &HostEndpoint,
        _identity: &HostKeyIdentity,
    ) -> AppResult<HostKeyStatus> {
        Ok(self.status.clone())
    }

    /// 记录任何持久信任写入，验证 probe 只允许内存 pin。
    async fn trust(&self, _endpoint: &HostEndpoint, _identity: &HostKeyIdentity) -> AppResult<()> {
        self.trust_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// 测试不执行密钥轮换；明确 Unsupported 以免把静态 fixture 状态误作持久信任。
    ///
    /// @author kongweiguang
    async fn replace(
        &self,
        _endpoint: &HostEndpoint,
        _expected_previous_fingerprint_sha256: &str,
        _identity: &HostKeyIdentity,
    ) -> AppResult<()> {
        Err(kerminal_application::AppError::unsupported(
            "test known_hosts fake does not support key rotation",
        ))
    }
}

/// 构造精确 endpoint/fingerprint 确认；peer 由调用者绑定到单一路由目标。
fn confirmation(endpoint: HostEndpoint, fingerprint: &str) -> ProbeHostKeyConfirmation {
    ProbeHostKeyConfirmation {
        peer: HostKeyPeer::Target,
        endpoint,
        fingerprint_sha256: fingerprint.to_owned(),
    }
}

/// 描述 SSH 主机公钥测试值，正文不会出现在测试输出中。
fn identity(fingerprint: &str) -> HostKeyIdentity {
    HostKeyIdentity {
        algorithm: "ssh-ed25519".to_owned(),
        fingerprint_sha256: fingerprint.to_owned(),
        public_key_openssh: "ssh-ed25519 AAAATEST".to_owned(),
    }
}

#[tokio::test]
async fn pin_is_exact_single_use_and_never_persisted() {
    let endpoint = HostEndpoint {
        host: "example.invalid".to_owned(),
        port: 22,
        username: "user".to_owned(),
    };
    let saved = Arc::new(FixedKnownHosts {
        status: HostKeyStatus::Unknown,
        trust_calls: AtomicUsize::new(0),
    });
    let repository = ProbeKnownHostsRepository {
        saved: saved.clone(),
        confirmation: confirmation(endpoint.clone(), "SHA256:accepted"),
        consumed: AtomicBool::new(false),
    };

    assert_eq!(
        repository
            .inspect(&endpoint, &identity("SHA256:accepted"))
            .await
            .expect("first exact check"),
        HostKeyStatus::Known
    );
    assert_eq!(
        repository
            .inspect(&endpoint, &identity("SHA256:accepted"))
            .await
            .expect("pin is consumed"),
        HostKeyStatus::Unknown
    );
    assert_eq!(
        repository
            .inspect(&endpoint, &identity("SHA256:different"))
            .await
            .expect("different fingerprint remains unknown"),
        HostKeyStatus::Unknown
    );
    assert!(
        repository
            .trust(&endpoint, &identity("SHA256:accepted"))
            .await
            .is_err()
    );
    assert_eq!(saved.trust_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn changed_key_is_never_overridden_by_temporary_confirmation() {
    let endpoint = HostEndpoint {
        host: "example.invalid".to_owned(),
        port: 22,
        username: "user".to_owned(),
    };
    let changed = HostKeyStatus::Changed {
        previous_fingerprint_sha256: "SHA256:old".to_owned(),
    };
    let repository = ProbeKnownHostsRepository {
        saved: Arc::new(FixedKnownHosts {
            status: changed.clone(),
            trust_calls: AtomicUsize::new(0),
        }),
        confirmation: confirmation(endpoint.clone(), "SHA256:new"),
        consumed: AtomicBool::new(false),
    };

    assert_eq!(
        repository
            .inspect(&endpoint, &identity("SHA256:new"))
            .await
            .expect("changed key remains blocked"),
        changed
    );
}

#[tokio::test]
async fn pin_does_not_apply_to_another_endpoint() {
    let confirmed = HostEndpoint {
        host: "first.example.invalid".to_owned(),
        port: 22,
        username: "user".to_owned(),
    };
    let other = HostEndpoint {
        host: "second.example.invalid".to_owned(),
        port: 22,
        username: "user".to_owned(),
    };
    let repository = ProbeKnownHostsRepository {
        saved: Arc::new(FixedKnownHosts {
            status: HostKeyStatus::Unknown,
            trust_calls: AtomicUsize::new(0),
        }),
        confirmation: confirmation(confirmed, "SHA256:accepted"),
        consumed: AtomicBool::new(false),
    };

    assert_eq!(
        repository
            .inspect(&other, &identity("SHA256:accepted"))
            .await
            .expect("another endpoint stays unknown"),
        HostKeyStatus::Unknown
    );
}

struct UnusedCredentials;

#[async_trait]
impl CredentialResolver for UnusedCredentials {
    /// 非 SSH 探测不得请求认证材料；若路由误入凭据解析则让测试明确失败。
    ///
    /// @author kongweiguang
    async fn resolve(
        &self,
        _reference: &kerminal_domain::connections::CredentialRef,
        _cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        Err(AppError::infrastructure("非 SSH 探测意外解析凭据"))
    }
}

struct ProbeControl {
    close_calls: AtomicUsize,
    failing_closes: usize,
}

#[async_trait]
impl SessionTransportControl for ProbeControl {
    /// 临时探测不调整尺寸；仅保留接口以复用生产 transport contract。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 按次数模拟 cleanup 可重试失败，验证 probe 不会丢弃底层 control owner。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        let call = self.close_calls.fetch_add(1, Ordering::AcqRel);
        if call < self.failing_closes {
            Err(AppError::infrastructure("测试 transport cleanup 暂时失败"))
        } else {
            Ok(())
        }
    }

    /// 测试 transport 没有子进程，仅提供无错误的最终退出收据。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

struct FakeTransportFactory {
    control: Arc<ProbeControl>,
    open_calls: AtomicUsize,
    last_target: StdMutex<Option<SessionTarget>>,
}

#[async_trait]
impl SessionTransportFactory for FakeTransportFactory {
    /// 只记录并返回内存 stream，避免测试拨号、访问设备或创建 SessionInfo。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        self.open_calls.fetch_add(1, Ordering::AcqRel);
        *self.last_target.lock().expect("fake target mutex") = Some(target.clone());
        Ok(SessionTransport {
            reader: Box::pin(tokio::io::empty()),
            writer: Box::pin(tokio::io::sink()),
            control: self.control.clone(),
            shell_dialect: RemoteShellDialect::Unknown,
            process_id: None,
            shell_integration_enabled: false,
        })
    }
}

struct RetryableOpenCleanup {
    close_calls: AtomicUsize,
}

#[async_trait]
impl SessionOpenCleanupOwner for RetryableOpenCleanup {
    /// 首次关闭失败以证明错误携带的 owner 会留到 probe shutdown 再次关闭。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        if self.close_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            Err(AppError::infrastructure("测试 open cleanup 暂时失败"))
        } else {
            Ok(())
        }
    }
}

struct FailedOpenFactory {
    cleanup: Arc<RetryableOpenCleanup>,
}

#[async_trait]
impl SessionTransportFactory for FailedOpenFactory {
    /// 模拟已取得资源后 open 失败，并把唯一清理 owner 交给调用方。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let cleanup: Arc<dyn SessionOpenCleanupOwner> = self.cleanup.clone();
        Err(SessionTransportOpenError::with_cleanup(
            AppError::infrastructure("测试 transport open 失败"),
            cleanup,
        ))
    }
}

struct CancellationWaitingFactory {
    started: Notify,
}

#[async_trait]
impl SessionTransportFactory for CancellationWaitingFactory {
    /// 等待 probe shutdown 发出 token，再以无资源错误结束 worker，固定取消与 join 顺序。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        self.started.notify_one();
        cancellation.cancelled().await;
        Err(SessionTransportOpenError::new(AppError::cancelled(
            "测试 probe 已取消",
        )))
    }
}

struct DelayedOpenFactory {
    started: Notify,
    release: Notify,
    control: Arc<ProbeControl>,
}

#[async_trait]
impl SessionTransportFactory for DelayedOpenFactory {
    /// 故意不响应取消，验证 shutdown 超时后 TaskTracker 仍拥有 worker 的清理 future。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(SessionTransport {
            reader: Box::pin(tokio::io::empty()),
            writer: Box::pin(tokio::io::sink()),
            control: self.control.clone(),
            shell_dialect: RemoteShellDialect::Unknown,
            process_id: None,
            shell_integration_enabled: false,
        })
    }
}

/// 为协议探测准备不指向真实设备或网络的最小目标模型。
///
/// @author kongweiguang
fn transient_target(protocol: RemoteProtocol) -> ConnectionProbeTarget {
    let serial = (protocol == RemoteProtocol::Serial)
        .then(|| SerialSessionTarget::new(std::path::PathBuf::from("probe-device")));
    ConnectionProbeTarget {
        profile: HostConnectionProfile {
            host_id: HostId::new(),
            protocol,
            endpoint: HostEndpoint {
                host: "127.0.0.1".to_owned(),
                port: 2323,
                username: String::new(),
            },
            authentication: None,
            rdp_password: None,
            route: SshRoute::default(),
            shell_dialect: RemoteShellDialect::Unknown,
            connect_timeout_ms: 1_000,
            keepalive_interval_ms: None,
        },
        sftp_remote_start_directory: (protocol == RemoteProtocol::Sftp)
            .then(|| RemotePath::parse("/").expect("static SFTP root path")),
        serial,
        timeout: Duration::from_secs(5),
        host_key_confirmation: None,
        transient_credentials: Vec::new(),
    }
}

/// Telnet probe 复用现有 factory 并直接关闭临时 transport，不创建应用级会话记录。
///
/// @author kongweiguang
#[tokio::test]
async fn telnet_probe_reuses_factory_and_closes_transient_transport() {
    let control = Arc::new(ProbeControl {
        close_calls: AtomicUsize::new(0),
        failing_closes: 0,
    });
    let factory = Arc::new(FakeTransportFactory {
        control: control.clone(),
        open_calls: AtomicUsize::new(0),
        last_target: StdMutex::new(None),
    });
    let backend = SystemConnectionProbe::new(
        Arc::new(UnusedCredentials),
        Arc::new(FixedKnownHosts {
            status: HostKeyStatus::Unknown,
            trust_calls: AtomicUsize::new(0),
        }),
        factory.clone(),
    );

    let outcome = backend
        .probe(
            transient_target(RemoteProtocol::Telnet),
            CancellationToken::new(),
        )
        .await
        .expect("fake Telnet transport should open and close");

    assert_eq!(
        outcome,
        ConnectionProbeOutcome::Verified(ProbeVerificationLevel::TelnetTransportConnected)
    );
    assert_eq!(factory.open_calls.load(Ordering::Acquire), 1);
    let target = factory
        .last_target
        .lock()
        .expect("fake target mutex")
        .clone();
    let Some(SessionTarget::Telnet(target)) = target else {
        panic!("Telnet probe must route through the injected terminal factory");
    };
    assert_eq!(target.host, "127.0.0.1");
    assert_eq!(target.port, 2323);
    assert_eq!(control.close_calls.load(Ordering::Acquire), 1);
}

/// 串口关闭首次失败时保留 control，应用 shutdown 用新总 deadline 重试并回收。
///
/// @author kongweiguang
#[tokio::test]
async fn failed_transport_close_is_retained_for_shutdown_retry() {
    let control = Arc::new(ProbeControl {
        close_calls: AtomicUsize::new(0),
        failing_closes: 1,
    });
    let factory = Arc::new(FakeTransportFactory {
        control: control.clone(),
        open_calls: AtomicUsize::new(0),
        last_target: StdMutex::new(None),
    });
    let backend = SystemConnectionProbe::new(
        Arc::new(UnusedCredentials),
        Arc::new(FixedKnownHosts {
            status: HostKeyStatus::Unknown,
            trust_calls: AtomicUsize::new(0),
        }),
        factory.clone(),
    );

    assert!(
        backend
            .probe(
                transient_target(RemoteProtocol::Serial),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(backend.pending_cleanup.owners.lock().await.len(), 1);
    let target = factory
        .last_target
        .lock()
        .expect("fake target mutex")
        .clone();
    let Some(SessionTarget::Serial(target)) = target else {
        panic!("Serial probe must route through the injected terminal factory");
    };
    assert_eq!(target.device, std::path::PathBuf::from("probe-device"));
    assert_eq!(target.baud_rate, 9600);
    assert_eq!(
        target.data_bits,
        kerminal_domain::sessions::SerialDataBits::Eight
    );
    assert_eq!(
        target.stop_bits,
        kerminal_domain::sessions::SerialStopBits::One
    );
    assert_eq!(target.parity, kerminal_domain::sessions::SerialParity::None);
    assert_eq!(
        target.flow_control,
        kerminal_domain::sessions::SerialFlowControl::None
    );

    backend
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("shutdown retry should release the retained control");
    assert_eq!(control.close_calls.load(Ordering::Acquire), 2);
    assert!(backend.pending_cleanup.owners.lock().await.is_empty());
}

/// open 失败并携带 owner 时，cleanup 失败不会丢失资源，shutdown 再次接管同一 owner。
///
/// @author kongweiguang
#[tokio::test]
async fn failed_open_owner_is_retained_until_shutdown() {
    let cleanup = Arc::new(RetryableOpenCleanup {
        close_calls: AtomicUsize::new(0),
    });
    let backend = SystemConnectionProbe::new(
        Arc::new(UnusedCredentials),
        Arc::new(FixedKnownHosts {
            status: HostKeyStatus::Unknown,
            trust_calls: AtomicUsize::new(0),
        }),
        Arc::new(FailedOpenFactory {
            cleanup: cleanup.clone(),
        }),
    );

    assert!(
        backend
            .probe(
                transient_target(RemoteProtocol::Telnet),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(cleanup.close_calls.load(Ordering::Acquire), 1);
    assert_eq!(backend.pending_cleanup.owners.lock().await.len(), 1);

    backend
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("shutdown should retry the open cleanup owner");
    assert_eq!(cleanup.close_calls.load(Ordering::Acquire), 2);
    assert!(backend.pending_cleanup.owners.lock().await.is_empty());
}

/// shutdown 封闭 intake、取消活动探测并等待 worker 收尾，避免迟到 owner 漏出 shutdown。
///
/// @author kongweiguang
#[tokio::test]
async fn shutdown_cancels_and_joins_probe_workers_before_draining_cleanup() {
    let factory = Arc::new(CancellationWaitingFactory {
        started: Notify::new(),
    });
    let backend = Arc::new(SystemConnectionProbe::new(
        Arc::new(UnusedCredentials),
        Arc::new(FixedKnownHosts {
            status: HostKeyStatus::Unknown,
            trust_calls: AtomicUsize::new(0),
        }),
        factory.clone(),
    ));
    let worker = {
        let backend = backend.clone();
        tokio::spawn(async move {
            backend
                .probe(
                    transient_target(RemoteProtocol::Telnet),
                    CancellationToken::new(),
                )
                .await
        })
    };
    factory.started.notified().await;

    backend
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("shutdown should wait for cancelled probe worker");
    assert!(worker.await.expect("probe join").is_err());
    assert!(
        backend
            .probe(
                transient_target(RemoteProtocol::Telnet),
                CancellationToken::new(),
            )
            .await
            .is_err(),
        "closed intake must reject new probes"
    );
}

/// shutdown deadline 到期只结束等待，不丢 task owner；后续释放后可用新 deadline 排空。
///
/// @author kongweiguang
#[tokio::test]
async fn shutdown_timeout_keeps_worker_tracked_for_a_later_drain() {
    let control = Arc::new(ProbeControl {
        close_calls: AtomicUsize::new(0),
        failing_closes: 0,
    });
    let factory = Arc::new(DelayedOpenFactory {
        started: Notify::new(),
        release: Notify::new(),
        control: control.clone(),
    });
    let backend = Arc::new(SystemConnectionProbe::new(
        Arc::new(UnusedCredentials),
        Arc::new(FixedKnownHosts {
            status: HostKeyStatus::Unknown,
            trust_calls: AtomicUsize::new(0),
        }),
        factory.clone(),
    ));
    let worker = {
        let backend = backend.clone();
        tokio::spawn(async move {
            backend
                .probe(
                    transient_target(RemoteProtocol::Telnet),
                    CancellationToken::new(),
                )
                .await
        })
    };
    factory.started.notified().await;

    assert!(
        backend.shutdown_until(Instant::now()).await.is_err(),
        "shutdown must report its expired absolute deadline"
    );
    factory.release.notify_one();
    assert!(worker.await.expect("probe join").is_err());

    backend
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("a later shutdown call should join the still tracked worker");
    assert_eq!(control.close_calls.load(Ordering::Acquire), 1);
}

/// 对 loopback 执行完整探测，断言本机状态与 TCP 状态分开返回且不启动 RDP 客户端。
///
/// @author kongweiguang
#[tokio::test]
async fn rdp_probe_reports_local_and_loopback_states_independently() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind loopback listener");
    let endpoint = HostEndpoint {
        host: "127.0.0.1".to_owned(),
        port: listener.local_addr().expect("listener address").port(),
        username: String::new(),
    };

    let outcome = probe_rdp_connectivity(
        &endpoint,
        &CancellationToken::new(),
        Instant::now() + Duration::from_secs(2),
    )
    .await
    .expect("loopback RDP connectivity probe should complete");

    let ConnectionProbeOutcome::RdpConnectivity(result) = outcome else {
        panic!("RDP probe must return independent local and remote states");
    };
    assert!(matches!(
        result.local_availability,
        RdpLocalAvailability::ClientAvailable(_)
            | RdpLocalAvailability::SystemLauncherAvailable { .. }
            | RdpLocalAvailability::Unavailable
    ));
    assert_eq!(result.remote_port, endpoint.port);
    assert_eq!(result.remote_tcp, RdpTcpReachability::Reachable);
    drop(listener);
}

/// Windows loopback 拒绝在验证机约 2.05 秒返回；测试单独留 5 秒预算，仍验证真实拒绝类别且不改生产 deadline。
///
/// @author kongweiguang
#[tokio::test]
async fn rdp_tcp_probe_reports_loopback_connection_refused() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("reserve loopback port");
    let port = listener.local_addr().expect("listener address").port();
    drop(listener);
    let endpoint = HostEndpoint {
        host: "127.0.0.1".to_owned(),
        port,
        username: String::new(),
    };

    let result = probe_rdp_tcp(
        &endpoint,
        &CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("connection refusal is a completed reachability result");

    assert_eq!(
        result,
        RdpTcpReachability::Unreachable(RdpTcpFailure::ConnectionRefused)
    );
}

/// 预取消令牌优先于连接 future，保证取消反馈不会被改写成网络错误。
///
/// @author kongweiguang
#[tokio::test]
async fn rdp_tcp_probe_preserves_cancellation() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = probe_rdp_tcp_with(
        std::future::pending::<std::io::Result<tokio::net::TcpStream>>(),
        &cancellation,
        Instant::now() + Duration::from_secs(1),
    )
    .await;

    assert_eq!(
        result.expect_err("cancelled probe must fail").public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
}

/// pending loopback测试 future 只用于隔离 deadline 行为，超时必须保留 timeout 错误类别。
///
/// @author kongweiguang
#[tokio::test]
async fn rdp_tcp_probe_reports_bounded_timeout() {
    let result = probe_rdp_tcp_with(
        std::future::pending::<std::io::Result<tokio::net::TcpStream>>(),
        &CancellationToken::new(),
        Instant::now() + Duration::from_millis(10),
    )
    .await;

    assert_eq!(
        result
            .expect_err("pending connect should hit its deadline")
            .public_code(),
        kerminal_application::PublicErrorCode::Timeout
    );
}

#[derive(Clone, Copy)]
enum SftpDirectoryFixtureMode {
    Accessible,
    Inaccessible,
    WaitForCancellation,
}

struct SftpDirectoryFixtureState {
    mode: SftpDirectoryFixtureMode,
    expected_path: String,
    requested_paths: StdMutex<Vec<String>>,
    read_directory_calls: AtomicUsize,
    read_directory_started: Notify,
    release_read_directory: Notify,
    handler_drop_count: AtomicUsize,
    handler_dropped: Notify,
    lease_close_count: AtomicUsize,
}

struct SftpDirectoryFixtureHandler {
    state: Arc<SftpDirectoryFixtureState>,
}

impl Drop for SftpDirectoryFixtureHandler {
    /// 测试服务在底层流 EOF 后通知等待方，确认取消确实释放了 SFTP channel。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.state.handler_drop_count.fetch_add(1, Ordering::AcqRel);
        self.state.handler_dropped.notify_one();
    }
}

impl russh_sftp::server::Handler for SftpDirectoryFixtureHandler {
    type Error = russh_sftp::server::StatusReply;

    /// 测试协议未覆盖的操作全部 fail closed，避免 fixture 隐式接受额外能力。
    ///
    /// @author kongweiguang
    fn unimplemented(&self) -> Self::Error {
        russh_sftp::protocol::StatusCode::OpUnsupported.into()
    }

    /// 记录客户端打开的精确路径，并按用例模拟目录不存在或权限拒绝。
    ///
    /// @author kongweiguang
    async fn opendir(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
        self.state
            .requested_paths
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(path.clone());
        if path != self.state.expected_path {
            return Err(russh_sftp::protocol::StatusCode::NoSuchFile.into());
        }
        if matches!(self.state.mode, SftpDirectoryFixtureMode::Inaccessible) {
            return Err(russh_sftp::protocol::StatusCode::PermissionDenied.into());
        }
        Ok(russh_sftp::protocol::Handle {
            id,
            handle: "fixture-directory".to_owned(),
        })
    }

    /// 在 SFTP READDIR 阶段提供 EOF，或等待取消用例释放，覆盖 session 收尾时机。
    ///
    /// @author kongweiguang
    async fn readdir(
        &mut self,
        _id: u32,
        _handle: String,
    ) -> Result<russh_sftp::protocol::Name, Self::Error> {
        self.state
            .read_directory_calls
            .fetch_add(1, Ordering::AcqRel);
        if matches!(
            self.state.mode,
            SftpDirectoryFixtureMode::WaitForCancellation
        ) {
            self.state.read_directory_started.notify_one();
            self.state.release_read_directory.notified().await;
        }
        Err(russh_sftp::protocol::StatusCode::Eof.into())
    }

    /// CLOSE 请求幂等确认；验证重点是 channel shutdown 与连接 lease 回收。
    ///
    /// @author kongweiguang
    async fn close(
        &mut self,
        id: u32,
        _handle: String,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        Ok(russh_sftp::protocol::Status {
            id,
            status_code: russh_sftp::protocol::StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }
}

struct SftpDirectoryFixtureHandle {
    host_id: HostId,
    state: Arc<SftpDirectoryFixtureState>,
}

#[async_trait]
impl RemoteConnectionHandle for SftpDirectoryFixtureHandle {
    /// 返回仅用于测试 lease 绑定的 host id。
    ///
    /// @author kongweiguang
    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// SFTP fixture 只开放文件能力，防止探测测试误用终端或隧道接口。
    ///
    /// @author kongweiguang
    fn capabilities(&self) -> ConnectionCapabilities {
        ConnectionCapabilities::for_protocol(RemoteProtocol::Sftp)
    }

    /// Fixture 不提供 shell 方言，确保测试不依赖远端命令执行。
    ///
    /// @author kongweiguang
    fn shell_dialect(&self) -> RemoteShellDialect {
        RemoteShellDialect::Unknown
    }

    /// 探测 fixture 不支持终端打开。
    ///
    /// @author kongweiguang
    async fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        Err(AppError::unsupported(
            "SFTP probe fixture terminal is unavailable",
        ))
    }

    /// 探测 fixture 不支持 exec 请求。
    ///
    /// @author kongweiguang
    async fn exec_stream(
        &self,
        _request: ExecRequest,
        _cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        Err(AppError::unsupported(
            "SFTP probe fixture exec is unavailable",
        ))
    }

    /// 探测 fixture 不支持原始 shell 请求。
    ///
    /// @author kongweiguang
    async fn exec_shell(
        &self,
        _request: ShellScriptRequest,
        _cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        Err(AppError::unsupported(
            "SFTP probe fixture shell is unavailable",
        ))
    }

    /// 为每个测试打开隔离 duplex channel，并运行真实锁定版 SFTP server 协议处理器。
    ///
    /// @author kongweiguang
    async fn open_sftp(
        &self,
        _request: OpenSftpRequest,
        cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("SFTP probe fixture open cancelled"));
        }
        let (client, server) = tokio::io::duplex(64 * 1024);
        russh_sftp::server::run(
            server,
            SftpDirectoryFixtureHandler {
                state: Arc::clone(&self.state),
            },
        )
        .await;
        Ok(SftpChannelHandle::new(Box::new(client)))
    }

    /// 探测 fixture 不支持 direct-tcpip。
    ///
    /// @author kongweiguang
    async fn open_tunnel(
        &self,
        _request: TunnelRequest,
        _cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        Err(AppError::unsupported(
            "SFTP probe fixture tunnel is unavailable",
        ))
    }

    /// 探测 fixture 不支持远端监听。
    ///
    /// @author kongweiguang
    async fn request_remote_forward(
        &self,
        _request: RemoteForwardRequest,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        Err(AppError::unsupported(
            "SFTP probe fixture forwarding is unavailable",
        ))
    }

    /// 记录探测 lease 的唯一关闭事实，便于失败和取消测试断言资源回收。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant, _reason: &str) -> AppResult<()> {
        self.state.lease_close_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct SftpDirectoryFixtureService {
    handle: Arc<SftpDirectoryFixtureHandle>,
}

#[async_trait]
impl RemoteConnectionService for SftpDirectoryFixtureService {
    /// 仅返回绑定到 fixture host 的 SFTP lease，不建立外部连接。
    ///
    /// @author kongweiguang
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("SFTP probe fixture connect cancelled"));
        }
        let handle: Arc<dyn RemoteConnectionHandle> = Arc::clone(&self.handle) as _;
        Ok(RemoteConnectionLease::new(handle))
    }

    /// 测试直接调用已认证阶段，host-key 检查不属于该 fixture 的责任。
    ///
    /// @author kongweiguang
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported(
            "SFTP probe fixture host-key inspection is unavailable",
        ))
    }

    /// Fixture 不修改 known_hosts。
    ///
    /// @author kongweiguang
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported(
            "SFTP probe fixture trust is unavailable",
        ))
    }

    /// Fixture 不支持 host-key 轮换。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: HostKeyRotationRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyRotationReceipt> {
        Err(AppError::unsupported(
            "SFTP probe fixture key rotation is unavailable",
        ))
    }
}

/// 构造只用于一个目录探测用例的内存 SFTP server 与 lease 生命周期记录器。
///
/// @author kongweiguang
fn sftp_directory_fixture(
    mode: SftpDirectoryFixtureMode,
    expected_path: &str,
) -> (
    Arc<SftpDirectoryFixtureService>,
    Arc<SftpDirectoryFixtureState>,
) {
    let state = Arc::new(SftpDirectoryFixtureState {
        mode,
        expected_path: expected_path.to_owned(),
        requested_paths: StdMutex::new(Vec::new()),
        read_directory_calls: AtomicUsize::new(0),
        read_directory_started: Notify::new(),
        release_read_directory: Notify::new(),
        handler_drop_count: AtomicUsize::new(0),
        handler_dropped: Notify::new(),
        lease_close_count: AtomicUsize::new(0),
    });
    let handle = Arc::new(SftpDirectoryFixtureHandle {
        host_id: HostId::new(),
        state: Arc::clone(&state),
    });
    (Arc::new(SftpDirectoryFixtureService { handle }), state)
}

/// 真实执行 SFTP INIT/OPENDIR/READDIR 并验证配置路径被读取后完成 session 与 lease 收尾。
///
/// @author kongweiguang
#[tokio::test]
async fn sftp_probe_requires_listable_start_directory_and_releases_resources() {
    let path = RemotePath::parse("/srv/kerminal").expect("valid fixture path");
    let (service, state) =
        sftp_directory_fixture(SftpDirectoryFixtureMode::Accessible, path.as_str());
    let target = transient_target(RemoteProtocol::Sftp);
    let result = connect_and_check_sftp(
        service.as_ref(),
        &target.profile,
        Some(&path),
        &CancellationToken::new(),
        ProbeDeadlines::from_timeout(Duration::from_secs(2)),
    )
    .await
    .expect("listable directory proves SFTP is ready");

    assert_eq!(
        result,
        ConnectionProbeOutcome::Verified(ProbeVerificationLevel::SftpDirectoryAccessible)
    );
    assert_eq!(
        *state
            .requested_paths
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
        [path.as_str()]
    );
    assert_eq!(state.read_directory_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.lease_close_count.load(Ordering::Acquire), 1);
    wait_for_sftp_fixture_channel_close(&state).await;
}

/// 远端拒绝打开起始目录时返回失败，并且仍关闭 SFTP session 与连接 lease。
///
/// @author kongweiguang
#[tokio::test]
async fn sftp_probe_rejects_inaccessible_start_directory_and_releases_resources() {
    let path = RemotePath::parse("/srv/private").expect("valid fixture path");
    let (service, state) =
        sftp_directory_fixture(SftpDirectoryFixtureMode::Inaccessible, path.as_str());
    let target = transient_target(RemoteProtocol::Sftp);
    let error = connect_and_check_sftp(
        service.as_ref(),
        &target.profile,
        Some(&path),
        &CancellationToken::new(),
        ProbeDeadlines::from_timeout(Duration::from_secs(2)),
    )
    .await
    .expect_err("permission denied directory must fail the probe");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(state.read_directory_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.lease_close_count.load(Ordering::Acquire), 1);
    wait_for_sftp_fixture_channel_close(&state).await;
}

/// 取消挂起的 READDIR 后先释放 session 与 lease，再释放 fixture handler，验证取消路径资源闭环。
///
/// @author kongweiguang
#[tokio::test]
async fn sftp_probe_cancellation_closes_session_and_lease() {
    let path = RemotePath::parse("/srv/waiting").expect("valid fixture path");
    let (service, state) =
        sftp_directory_fixture(SftpDirectoryFixtureMode::WaitForCancellation, path.as_str());
    let target = transient_target(RemoteProtocol::Sftp);
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker_path = path.clone();
    let worker = tokio::spawn(async move {
        connect_and_check_sftp(
            service.as_ref(),
            &target.profile,
            Some(&worker_path),
            &worker_cancellation,
            ProbeDeadlines::from_timeout(Duration::from_secs(3)),
        )
        .await
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        state.read_directory_started.notified(),
    )
    .await
    .expect("fixture reached READDIR");
    cancellation.cancel();

    let error = tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("cancelled SFTP probe finishes cleanup")
        .expect("probe worker joins")
        .expect_err("cancellation must not report directory success");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    assert_eq!(state.lease_close_count.load(Ordering::Acquire), 1);

    assert_eq!(state.read_directory_calls.load(Ordering::Acquire), 1);
    state.release_read_directory.notify_one();
    wait_for_sftp_fixture_channel_close(&state).await;
}

/// 有界等待 SFTP 对端 handler 因 session 关闭而释放，避免只断言 lease 计数漏掉 channel 泄漏。
///
/// @author kongweiguang
async fn wait_for_sftp_fixture_channel_close(state: &SftpDirectoryFixtureState) {
    tokio::time::timeout(Duration::from_secs(2), state.handler_dropped.notified())
        .await
        .expect("closing the SFTP session releases its server-side channel");
    assert_eq!(state.handler_drop_count.load(Ordering::Acquire), 1);
}
