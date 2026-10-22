//! OpenSSH SFTP 验收夹具；仅持有任务私有 endpoint 和随机远端工作目录。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    error::Error,
    fs,
    future::Future,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant as WallClockInstant},
};

use kerminal_application::{
    AppError, AppResult,
    connections::management::{
        HostAuthenticationMode, HostDraft, HostGroupDeletionReceipt, HostGroupDraft,
        HostGroupSnapshot, HostManagementRepository, HostManagementService, HostRouteSettings,
        HostSftpOptions, HostTerminalOptions, ManagedHost, RdpDisplayOptions,
    },
    connections::{
        ChannelLane, ConnectionIntent, CredentialResolver, HostConnectionRepository,
        OpenSftpRequest, RemoteConnectionService,
    },
    files::FileService,
    preferences::SftpPerformanceSettings,
    transfers::{
        InMemoryTransferRecoveryRepository, SftpTransferPolicyStore, TransferQueueOptions,
        TransferService,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol, RemoteShellDialect,
        SshAuthentication, SshJumpHost, SshRoute,
    },
    files::{
        ConflictPolicy, DeleteTargetKind, FileEndpoint, FileEntryKind, RemotePath,
        TransferFailureKind, TransferKind, TransferRequest, TransferStatus, WriteTextRequest,
        WriteTextStrategy,
    },
    identity::{HostId, TransferId},
    preferences::ConfigRevision,
};
use kerminal_infrastructure::{
    connections::{
        KnownHostsStore, RusshRemoteConnectionDependencies, RusshRemoteConnectionService,
    },
    files::{JsonFileBookmarkRepository, LocalFileProvider, SftpFileProvider, SftpTransferBackend},
    storage::StorageCoordinator,
};
use russh_sftp::client::{Config as SftpClientConfig, SftpSession};
use tokio::{
    runtime::Handle,
    sync::RwLock,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::support::ssh_harness::TestCredentialResolver;

pub(super) type AcceptanceResult<T = ()> = Result<T, Box<dyn Error>>;

const EXPECTED_FIXTURE_TARGET: &str = "127.0.0.1:22661";
const EXPECTED_FIXTURE_JUMP_PORTS: [u16; 2] = [22662, 22663];
const EXPECTED_FIXTURE_USERNAME: &str = "kong";
const KEY_REFERENCE: &str = "acceptance:sftp-client-key";
pub(super) const LARGE_FILE_BYTES: u64 = 128 * 1024 * 1024;
pub(super) const CANCELLATION_CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const OPERATION_TIMEOUT: Duration = Duration::from_secs(180);
pub(super) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// 为验收步骤打印无敏感数据的边界与耗时，让外部 watchdog 超时可定位到最后一步。
///
/// @author kongweiguang
pub(super) async fn run_stage<T>(
    label: &str,
    future: impl Future<Output = AcceptanceResult<T>>,
) -> AcceptanceResult<T> {
    let started = stage_begin(label);
    let result = future.await;
    stage_end(label, started, result.is_ok());
    result
}

/// 记录固定阶段标签的开始时刻，不包含 endpoint、路径或认证材料。
///
/// @author kongweiguang
fn stage_begin(label: &str) -> WallClockInstant {
    eprintln!("[sftp-acceptance] stage={label} event=begin");
    WallClockInstant::now()
}

/// 输出阶段终态及墙钟耗时；失败详情仍由原错误链按既有安全策略返回。
///
/// @author kongweiguang
fn stage_end(label: &str, started: WallClockInstant, succeeded: bool) {
    let result = if succeeded { "ok" } else { "failed" };
    eprintln!(
        "[sftp-acceptance] stage={label} event=end result={result} elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

/// 输出队列权威快照中已有的安全进度字段，不伪造尚未暴露的内部传输 phase。
///
/// @author kongweiguang
fn log_transfer_snapshot(
    stage: &str,
    event: &str,
    elapsed: Duration,
    summary: &kerminal_domain::files::TransferSummary,
) {
    let total = summary
        .total_bytes
        .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string());
    eprintln!(
        "[sftp-acceptance] stage={stage} event={event} elapsed_ms={} status={:?} bytes={}/{} cleanup_pending={}",
        elapsed.as_millis(),
        summary.status,
        summary.bytes_transferred,
        total,
        summary.cleanup_pending
    );
}

mod files;
pub(super) use files::exercise_local_source_symlink_policy;
mod remote_to_remote;
mod resume;
pub(super) struct OpenSshFixtureConfig {
    target: SocketAddr,
    jump_endpoints: [SocketAddr; 2],
    username: String,
    private_key_file: PathBuf,
    known_hosts_file: PathBuf,
    remote_root: RemotePath,
}

impl OpenSshFixtureConfig {
    /// 只接受任务指定的 loopback SFTP fixture，阻止忽略测试误连用户主机或生产数据。
    pub(super) fn from_environment() -> AcceptanceResult<Self> {
        let target: SocketAddr = std::env::var("KERM_ACCEPT_SSH_TARGET")?.parse()?;
        let expected: SocketAddr = EXPECTED_FIXTURE_TARGET.parse()?;
        if target != expected || !target.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SFTP acceptance requires the task-private loopback fixture",
            )
            .into());
        }
        let jump_endpoints =
            parse_jump_endpoints(&std::env::var("KERM_ACCEPT_SSH_JUMP_ENDPOINTS")?, target)?;
        let username = std::env::var("KERM_ACCEPT_SSH_USERNAME")?;
        if username != EXPECTED_FIXTURE_USERNAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SFTP fixture username does not match the task fixture",
            )
            .into());
        }
        let remote_root = std::env::var("KERM_ACCEPT_SSH_REMOTE_ROOT")?;
        let remote_prefix = format!("/home/{username}/.cache/kerminal-gpui-acceptance-ssh.");
        if !remote_root.starts_with(&remote_prefix)
            || !remote_root.ends_with("/remote-root")
            || remote_root.contains("/../")
            || remote_root == "/"
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SFTP fixture remote root is outside its task-private namespace",
            )
            .into());
        }
        let fixture_root = required_path("KERM_ACCEPT_SSH_FIXTURE_ROOT")?;
        let private_key_file = required_path("KERM_ACCEPT_SSH_PRIVATE_KEY_FILE")?;
        let known_hosts_file = required_path("KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE")?;
        if !private_key_file.starts_with(&fixture_root)
            || !known_hosts_file.starts_with(&fixture_root)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SFTP fixture credentials are outside the task-private fixture root",
            )
            .into());
        }
        Ok(Self {
            target,
            jump_endpoints,
            username,
            private_key_file,
            known_hosts_file,
            remote_root: RemotePath::parse(&remote_root)
                .map_err(|_| io::Error::other("SFTP fixture remote root is not a valid path"))?,
        })
    }
}

/// 解析且限制两个任务私有跳板，避免验收误连非 loopback 主机或退化成单跳测试。
///
/// @author kongweiguang
fn parse_jump_endpoints(value: &str, target: SocketAddr) -> io::Result<[SocketAddr; 2]> {
    let endpoints = value
        .split(',')
        .map(|part| {
            part.trim().parse::<SocketAddr>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SFTP acceptance jump endpoint is invalid",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if endpoints.len() != 2
        || endpoints
            .iter()
            .any(|endpoint| endpoint.ip() != target.ip() || *endpoint == target)
        || endpoints[0].port() != EXPECTED_FIXTURE_JUMP_PORTS[0]
        || endpoints[1].port() != EXPECTED_FIXTURE_JUMP_PORTS[1]
        || endpoints[0] == endpoints[1]
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SFTP acceptance requires two distinct loopback jump endpoints",
        ));
    }
    Ok([endpoints[0], endpoints[1]])
}

/// 测试配置仓库仅暴露显式登记的两个 HostId，不按 endpoint 或配置顺序猜测目标。
struct AcceptanceHostRepository {
    profiles: HashMap<HostId, HostConnectionProfile>,
}

impl AcceptanceHostRepository {
    /// 使用独立 HostId 建立双 remote fixture，拒绝重名身份导致连接配置被覆盖。
    ///
    /// @author kongweiguang
    fn new(profiles: Vec<HostConnectionProfile>) -> AppResult<Self> {
        let mut indexed = HashMap::with_capacity(profiles.len());
        for profile in profiles {
            if indexed.insert(profile.host_id.clone(), profile).is_some() {
                return Err(AppError::conflict(
                    "duplicate SFTP acceptance host identity",
                ));
            }
        }
        Ok(Self { profiles: indexed })
    }
}

#[async_trait::async_trait]
impl HostConnectionRepository for AcceptanceHostRepository {
    /// 按 HostId 返回准确 profile，避免跨主机传输把 source 和 destination 静默串线。
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        self.profiles
            .get(host_id)
            .cloned()
            .ok_or_else(|| AppError::not_found("SFTP acceptance host does not exist"))
    }
}

/// 为真实传输队列提供同一 HostId 的冻结主机 SFTP 选项；SSH route 仍由上面的 runtime repo 所有。
struct AcceptanceHostManagementRepository {
    hosts: RwLock<HashMap<HostId, ManagedHost>>,
}

impl AcceptanceHostManagementRepository {
    /// 只从任务私有连接快照建立内存管理视图，避免测试读取或写入用户主机配置。
    ///
    /// @author kongweiguang
    fn new(profiles: Vec<HostConnectionProfile>) -> AppResult<Self> {
        let mut hosts = HashMap::with_capacity(profiles.len());
        for profile in profiles {
            let host = fixture_managed_host(&profile);
            if hosts.insert(host.id.clone(), host).is_some() {
                return Err(AppError::conflict(
                    "duplicate SFTP acceptance host identity",
                ));
            }
        }
        Ok(Self {
            hosts: RwLock::new(hosts),
        })
    }

    /// 在下一笔入队前更新内存策略，使验收走真实 queue snapshot 而非绕过队列调用后端。
    ///
    /// @author kongweiguang
    async fn set_sftp_options(&self, host_id: &HostId, options: HostSftpOptions) -> AppResult<()> {
        let mut hosts = self.hosts.write().await;
        let host = hosts
            .get_mut(host_id)
            .ok_or_else(|| AppError::not_found("SFTP acceptance host does not exist"))?;
        host.sftp_options = options;
        Ok(())
    }
}

/// 传输验收只消费 host SFTP 选项；连接 profile 与 SSH route 继续由独立 runtime repo 提供。
///
/// @author kongweiguang
fn fixture_managed_host(profile: &HostConnectionProfile) -> ManagedHost {
    ManagedHost {
        id: profile.host_id.clone(),
        group_id: None,
        name: "Task-private OpenSSH acceptance".to_owned(),
        note: None,
        protocol: profile.protocol,
        endpoint: profile.endpoint.clone(),
        authentication: profile.authentication.clone(),
        authentication_mode: HostAuthenticationMode::PrivateKey,
        rdp_password: profile.rdp_password.clone(),
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: profile.connect_timeout_ms,
        keepalive_interval_ms: profile.keepalive_interval_ms,
        terminal_options: HostTerminalOptions::default(),
        sftp_options: HostSftpOptions::default(),
        rdp_display: RdpDisplayOptions::default(),
        created_at: "fixture".to_owned(),
        updated_at: "fixture".to_owned(),
        revision: ConfigRevision::from_sha256("sftp-acceptance-fixture".to_owned()),
    }
}

#[async_trait::async_trait]
impl HostManagementRepository for AcceptanceHostManagementRepository {
    /// 内存夹具没有持久化写路径，且队列只执行读取；不引入配置副作用。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Ok(())
    }

    /// 返回任务私有 HostId 对应的策略快照，不提供系统配置或用户数据。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(self.hosts.read().await.values().cloned().collect())
    }

    /// 队列按精确 HostId 冻结策略，未知身份明确失败而不猜测其它 fixture host。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self.hosts.read().await.get(host_id).cloned())
    }

    /// 该 fixture 仅支持读取和显式策略切换，拒绝伪装成主机配置持久化实现。
    async fn save_host(
        &self,
        _host_id: HostId,
        _draft: HostDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported(
            "SFTP acceptance host settings are not persisted",
        ))
    }

    /// 避免验收 fixture 通过配置管理端口删除任何主机。
    async fn delete_host(
        &self,
        _host_id: &HostId,
        _expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported(
            "SFTP acceptance host deletion is not supported",
        ))
    }

    /// 验收不使用分组配置，返回空的内存快照。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot::default())
    }

    /// 阻止 fixture 承担与传输策略无关的分组写入。
    async fn save_group(
        &self,
        _draft: HostGroupDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported(
            "SFTP acceptance group editing is not supported",
        ))
    }

    /// 阻止 fixture 承担主机配置事务或组引用迁移。
    async fn delete_group_and_reassign(
        &self,
        _group_id: String,
        _destination_group_id: Option<String>,
        _expected_group_revision: ConfigRevision,
        _expected_hosts: Vec<kerminal_application::connections::management::HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported(
            "SFTP acceptance group migration is not supported",
        ))
    }
}
/// 链接仅建在 source fixture 目录；OpenSSH 的 SFTP v3 SYMLINK wire 顺序要求反转客户端参数。
/// 目标限 source/target 随机工作区，文件传输仍由真实队列执行。
///
/// @author kongweiguang
pub(super) async fn create_owned_remote_symlink(
    fixture: &SftpAcceptanceFixture,
    host_id: &HostId,
    link: &RemotePath,
    target: &RemotePath,
) -> AcceptanceResult {
    let is_within = |root: &str, candidate: &str| {
        candidate == root
            || candidate
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with('/'))
    };
    let link_is_owned = is_within(fixture.source_workspace.as_str(), link.as_str());
    let target_is_owned = is_within(fixture.source_workspace.as_str(), target.as_str())
        || is_within(fixture.workspace.as_str(), target.as_str());
    if !link_is_owned || !target_is_owned {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SFTP acceptance symlink path is outside its owned fixture roots",
        )
        .into());
    }
    let cancellation = CancellationToken::new();
    let lease = fixture
        .connections
        .connect(
            host_id.clone(),
            ConnectionIntent::Sftp,
            cancellation.clone(),
        )
        .await?;
    let channel = lease
        .open_sftp(
            OpenSftpRequest {
                lane: ChannelLane::Bulk,
            },
            cancellation,
        )
        .await?;
    let session = SftpSession::new_with_config(channel, SftpClientConfig::default()).await?;
    let operation = timeout(
        OPERATION_TIMEOUT,
        session.symlink(target.as_str().to_owned(), link.as_str().to_owned()),
    )
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "fixture SFTP link creation timed out",
        )
    })
    .and_then(|result| result.map_err(io::Error::other));
    let close = timeout(OPERATION_TIMEOUT, session.close())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture SFTP session close timed out",
            )
        })
        .and_then(|result| result.map_err(io::Error::other));
    match (operation, close) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary.into()),
        (Ok(()), Err(cleanup)) => Err(cleanup.into()),
        (Err(primary), Err(cleanup)) => Err(io::Error::other(format!(
            "fixture SFTP link creation failed ({primary}); channel close also failed ({cleanup})"
        ))
        .into()),
    }
}

/// 将已验证的 loopback 地址构造为不引用系统 SSH 配置的测试 endpoint。
///
/// @author kongweiguang
fn fixture_endpoint(address: SocketAddr, username: &str) -> HostEndpoint {
    HostEndpoint {
        host: address.ip().to_string(),
        port: address.port(),
        username: username.to_owned(),
    }
}

/// 给每个远端独立创建身份，并把给定 SSH route 固定在 profile 上。
///
/// @author kongweiguang
fn fixture_profile(
    endpoint: HostEndpoint,
    authentication: SshAuthentication,
    route: SshRoute,
) -> HostConnectionProfile {
    HostConnectionProfile {
        host_id: HostId::new(),
        protocol: RemoteProtocol::Ssh,
        endpoint,
        authentication: Some(authentication),
        rdp_password: None,
        route,
        shell_dialect: RemoteShellDialect::Posix,
        connect_timeout_ms: 20_000,
        keepalive_interval_ms: None,
    }
}

pub(super) struct SftpAcceptanceFixture {
    connections: Arc<RusshRemoteConnectionService>,
    files: FileService,
    transfers: TransferService,
    host_management: Arc<AcceptanceHostManagementRepository>,
    host_id: HostId,
    source_host_id: HostId,
    workspace: RemotePath,
    source_workspace: RemotePath,
    workspace_created: AtomicBool,
    source_workspace_created: AtomicBool,
    local_root: tempfile::TempDir,
    _known_hosts_root: tempfile::TempDir,
}

impl SftpAcceptanceFixture {
    /// 把临时 key、known_hosts 和文件根注入真实 russh/SFTP service，不接触用户 SSH 或配置目录。
    pub(super) fn new(config: &OpenSshFixtureConfig) -> AcceptanceResult<Self> {
        let private_key = Zeroizing::new(fs::read(&config.private_key_file)?);
        if private_key.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "fixture key is empty").into());
        }
        let resolver = Arc::new(TestCredentialResolver::default());
        resolver.insert_private_key(KEY_REFERENCE, private_key.as_slice());
        let credentials: Arc<dyn CredentialResolver> = resolver;

        let known_hosts_root = tempfile::tempdir()?;
        fs::copy(
            &config.known_hosts_file,
            known_hosts_root.path().join("known_hosts"),
        )?;
        let known_hosts = Arc::new(KnownHostsStore::new(
            StorageCoordinator::new(known_hosts_root.path()),
            false,
        ));

        let authentication = SshAuthentication::PrivateKey {
            path: None,
            secret: Some(
                CredentialRef::new(KEY_REFERENCE)
                    .map_err(|_| io::Error::other("invalid fixture credential reference"))?,
            ),
            passphrase: None,
        };
        let [first_jump, second_jump] = config.jump_endpoints;
        let source_profile = fixture_profile(
            fixture_endpoint(first_jump, &config.username),
            authentication.clone(),
            SshRoute::default(),
        );
        let target_profile = fixture_profile(
            fixture_endpoint(config.target, &config.username),
            authentication.clone(),
            SshRoute {
                proxy: None,
                jump_hosts: vec![
                    SshJumpHost {
                        endpoint: fixture_endpoint(first_jump, &config.username),
                        authentication: authentication.clone(),
                    },
                    SshJumpHost {
                        endpoint: fixture_endpoint(second_jump, &config.username),
                        authentication,
                    },
                ],
            },
        );
        let host_id = target_profile.host_id.clone();
        let source_host_id = source_profile.host_id.clone();
        let host_management = Arc::new(AcceptanceHostManagementRepository::new(vec![
            target_profile.clone(),
            source_profile.clone(),
        ])?);
        let hosts = Arc::new(AcceptanceHostRepository::new(vec![
            target_profile,
            source_profile,
        ])?);
        let connections = Arc::new(RusshRemoteConnectionService::new(
            RusshRemoteConnectionDependencies {
                hosts,
                credentials,
                known_hosts,
            },
        ));
        let remote: Arc<dyn RemoteConnectionService> = connections.clone();
        let local_root = tempfile::tempdir()?;
        let files = FileService::new(
            vec![
                Arc::new(LocalFileProvider),
                Arc::new(SftpFileProvider::new(Arc::clone(&remote))),
            ],
            Arc::new(JsonFileBookmarkRepository::new(local_root.path())),
        )?;
        let transfers = TransferService::new_with_recovery_repository(
            vec![Arc::new(SftpTransferBackend::new(remote))],
            Handle::current(),
            TransferQueueOptions::default(),
            SftpTransferPolicyStore::new(SftpPerformanceSettings::default())?,
            Arc::new(InMemoryTransferRecoveryRepository::default()),
            Some(Arc::new(HostManagementService::new(
                host_management.clone(),
            ))),
        )?;
        let workspace = config
            .remote_root
            .join_name(&format!("sftp-acceptance-{}", Uuid::new_v4().simple()))
            .map_err(|_| io::Error::other("could not allocate isolated remote workspace"))?;
        let source_workspace = config
            .remote_root
            .join_name(&format!(
                "sftp-acceptance-source-{}",
                Uuid::new_v4().simple()
            ))
            .map_err(|_| io::Error::other("could not allocate isolated source workspace"))?;
        Ok(Self {
            connections,
            files,
            transfers,
            host_management,
            host_id,
            source_host_id,
            workspace,
            source_workspace,
            workspace_created: AtomicBool::new(false),
            source_workspace_created: AtomicBool::new(false),
            local_root,
            _known_hosts_root: known_hosts_root,
        })
    }

    /// 仅在 worker 与暂存清理均确认结束后删除测试目录；提交未知或清理失败时保留现场。
    ///
    /// @author kongweiguang
    pub(super) async fn cleanup(&self) -> AcceptanceResult {
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut cleanup_errors = Vec::new();
        let shutdown_started = stage_begin("cleanup.transfer_queue_shutdown");
        let shutdown_result = self.transfers.shutdown_until(deadline).await;
        stage_end(
            "cleanup.transfer_queue_shutdown",
            shutdown_started,
            shutdown_result.is_ok(),
        );
        let queue_shutdown_confirmed = match shutdown_result {
            Ok(()) => true,
            Err(error) => {
                cleanup_errors.push(format!("transfer queue: {}", error.public_code().as_str()));
                false
            }
        };
        let list_started = stage_begin("cleanup.list_before_owner_retry");
        let tasks_before_retry = self.transfers.list().await;
        stage_end("cleanup.list_before_owner_retry", list_started, true);
        let mut pending_owner_index = 0;
        for task in tasks_before_retry {
            if task.cleanup_pending {
                pending_owner_index += 1;
                let stage = format!("cleanup.retry_pending_owner_{pending_owner_index}");
                let retry_started = stage_begin(&stage);
                let retry_result = self.transfers.retry_cleanup_until(&task.id, deadline).await;
                stage_end(&stage, retry_started, retry_result.is_ok());
                if let Err(error) = retry_result {
                    cleanup_errors.push(format!(
                        "transfer cleanup: {}",
                        error.public_code().as_str()
                    ));
                }
            }
        }
        let snapshot_started = stage_begin("cleanup.verify_transfer_owners");
        let tasks = self.transfers.list().await;
        let pending_cleanup_count = tasks.iter().filter(|task| task.cleanup_pending).count();
        let unknown_commit_count = tasks
            .iter()
            .filter(|task| task.failure_kind == Some(TransferFailureKind::CommitUnknown))
            .count();
        eprintln!(
            "[sftp-acceptance] stage=cleanup.transfer_owner_snapshot event=summary tasks={} cleanup_pending={} commit_unknown={}",
            tasks.len(),
            pending_cleanup_count,
            unknown_commit_count
        );
        stage_end("cleanup.verify_transfer_owners", snapshot_started, true);
        let tasks_are_terminal_and_clean = tasks.iter().all(|task| {
            !task.cleanup_pending
                && matches!(
                    task.status,
                    TransferStatus::Succeeded
                        | TransferStatus::Skipped
                        | TransferStatus::Unchanged
                        | TransferStatus::Failed
                        | TransferStatus::Cancelled
                )
        });
        let transfer_resources_confirmed = queue_shutdown_confirmed && tasks_are_terminal_and_clean;
        if !transfer_resources_confirmed {
            cleanup_errors.push(
                "remote test workspaces and SSH owner retained because transfer cleanup is unconfirmed"
                    .to_owned(),
            );
        } else {
            let commit_outcome_known = tasks
                .iter()
                .all(|task| task.failure_kind != Some(TransferFailureKind::CommitUnknown));
            if !commit_outcome_known {
                cleanup_errors.push(
                    "remote test workspaces retained because a transfer commit outcome is unknown"
                        .to_owned(),
                );
            } else {
                if self.workspace_created.load(Ordering::Acquire) {
                    let remove_started = stage_begin("cleanup.remove_target_workspace");
                    let remove_result = remove_remote_tree(
                        &self.files,
                        remote_endpoint(&self.host_id, &self.workspace),
                    )
                    .await;
                    stage_end(
                        "cleanup.remove_target_workspace",
                        remove_started,
                        remove_result.is_ok(),
                    );
                    if let Err(error) = remove_result {
                        cleanup_errors.push(format!(
                            "remote test directory: {}",
                            error.public_code().as_str()
                        ));
                    }
                }
                if self.source_workspace_created.load(Ordering::Acquire) {
                    let remove_started = stage_begin("cleanup.remove_source_workspace");
                    let remove_result = remove_remote_tree(
                        &self.files,
                        remote_endpoint(&self.source_host_id, &self.source_workspace),
                    )
                    .await;
                    stage_end(
                        "cleanup.remove_source_workspace",
                        remove_started,
                        remove_result.is_ok(),
                    );
                    if let Err(error) = remove_result {
                        cleanup_errors.push(format!(
                            "remote source test directory: {}",
                            error.public_code().as_str()
                        ));
                    }
                }
            }
            let connection_shutdown_started = stage_begin("cleanup.ssh_connection_owner_shutdown");
            let connection_shutdown = self.connections.shutdown_until(deadline).await;
            stage_end(
                "cleanup.ssh_connection_owner_shutdown",
                connection_shutdown_started,
                connection_shutdown.is_ok(),
            );
            if let Err(error) = connection_shutdown {
                cleanup_errors.push(format!(
                    "SSH connection owner: {}",
                    error.public_code().as_str()
                ));
            }
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(cleanup_errors.join("; ")).into())
        }
    }
}
/// 仅创建目标随机工作区，使独立本机源链接验收也能复用真实队列并由 fixture 精确清理。
///
/// @author kongweiguang
pub(super) async fn prepare_target_workspace(fixture: &SftpAcceptanceFixture) -> AcceptanceResult {
    fixture
        .files
        .create_directory(
            CancellationToken::new(),
            remote_endpoint(&fixture.host_id, &fixture.workspace),
        )
        .await?;
    // 只有本任务随机目录成功创建后，teardown 才获得递归删除权限。
    fixture.workspace_created.store(true, Ordering::Release);
    Ok(())
}

/// 完整场景复用目标工作区准备入口；阶段输出区分慢传输与停滞，避免独立链接验收复制工作区所有权逻辑。
///
/// @author kongweiguang
pub(super) async fn exercise_sftp(fixture: &SftpAcceptanceFixture) -> AcceptanceResult {
    run_stage("workspace.create_target", prepare_target_workspace(fixture)).await?;
    run_stage("workspace.create_source", async {
        fixture
            .files
            .create_directory(
                CancellationToken::new(),
                remote_endpoint(&fixture.source_host_id, &fixture.source_workspace),
            )
            .await?;
        // 源和目标由独立随机目录隔离，尽管三个 sshd 共享同一 Unix 文件系统。
        fixture
            .source_workspace_created
            .store(true, Ordering::Release);
        Ok(())
    })
    .await?;

    run_stage(
        "files.empty_binary_atomic_overwrite_and_download",
        files::exercise_empty_binary_and_atomic_overwrite(fixture),
    )
    .await?;
    run_stage(
        "files.mtime_and_remote_source_symlink_policy",
        files::exercise_mtime_and_remote_source_symlink_policy(fixture),
    )
    .await?;
    run_stage(
        "files.directory_merge",
        files::exercise_directory_merge(fixture),
    )
    .await?;
    run_stage(
        "files.editor_revision_conflict",
        files::exercise_editor_revision_conflict(fixture),
    )
    .await?;
    run_stage(
        "remote_to_remote.scenarios",
        remote_to_remote::exercise_remote_to_remote(fixture),
    )
    .await?;
    run_stage(
        "resume.cancel_retry_and_download",
        resume::exercise_cancel_and_trusted_resume(fixture),
    )
    .await?;
    Ok(())
}
/// 将真实 transfer task 运行至终态；诊断只输出安全摘要和已确认提交计数，不泄露路径或凭据。
///
/// @author kongweiguang
async fn run_transfer(
    fixture: &SftpAcceptanceFixture,
    stage: &'static str,
    request: TransferRequest,
) -> AcceptanceResult<kerminal_domain::files::TransferSummary> {
    run_stage(stage, async {
    let source_kind = match &request.source {
        FileEndpoint::Local { .. } => "local",
        FileEndpoint::Remote { .. } => "remote",
        FileEndpoint::Container { .. } => "container",
    };
    let destination_kind = match &request.destination {
        FileEndpoint::Local { .. } => "local",
        FileEndpoint::Remote { .. } => "remote",
        FileEndpoint::Container { .. } => "container",
    };
    let transfer_kind = format!("{:?}", request.kind);
    let conflict_policy = format!("{:?}", request.conflict_policy);
    let initial = fixture.transfers.enqueue(request).await?;
    eprintln!("[sftp-acceptance] stage={stage} event=enqueued");
    let summary = wait_for_terminal(&fixture.transfers, &initial.id, stage).await?;
    if !matches!(
        summary.status,
        TransferStatus::Succeeded | TransferStatus::Unchanged
    ) {
        return Err(io::Error::other(format!(
            "SFTP task {} ended as {:?}; transfer={}; route={source_kind}->{destination_kind}; conflict_policy={conflict_policy}; failure={:?}; retryable={}; resumable={}; bytes={}/ {:?}; confirmed_entries={}; confirmed_bytes={}; cleanup_pending={}; safe_error={}",
            summary.id,
            summary.status,
            transfer_kind,
            summary.failure_kind,
            summary.retryable,
            summary.resumable,
            summary.bytes_transferred,
            summary.total_bytes,
            summary.commit_summary.committed_entries(),
            summary.commit_summary.committed_bytes(),
            summary.cleanup_pending,
            summary.safe_error.as_deref().unwrap_or("<none>")
        ))
        .into());
    }
    if summary.cleanup_pending {
        let cleanup_stage = format!("{stage}.cleanup_owner_retry");
        run_stage(&cleanup_stage, async {
            fixture
                .transfers
                .retry_cleanup_until(&summary.id, Instant::now() + Duration::from_secs(20))
                .await?;
            Ok(())
        })
        .await?;
    }
    Ok(summary)
    })
    .await
}

/// 仅在队列返回已终结摘要后继续，避免断言读取未完成传输的中间状态。
///
/// @author kongweiguang
async fn wait_for_terminal(
    transfers: &TransferService,
    id: &TransferId,
    stage: &str,
) -> AcceptanceResult<kerminal_domain::files::TransferSummary> {
    let wait_stage = format!("{stage}.await_terminal");
    let wait_started = stage_begin(&wait_stage);
    let started = WallClockInstant::now();
    let result = async {
        let summary = timeout(OPERATION_TIMEOUT, async {
            let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
            loop {
                let summary = transfers.get(id).await?;
                if matches!(
                    summary.status,
                    TransferStatus::Succeeded
                        | TransferStatus::Unchanged
                        | TransferStatus::Skipped
                        | TransferStatus::Failed
                        | TransferStatus::Cancelled
                ) {
                    log_transfer_snapshot(&wait_stage, "terminal", started.elapsed(), &summary);
                    return Ok::<_, AppError>(summary);
                }
                let now = Instant::now();
                if now >= next_heartbeat {
                    log_transfer_snapshot(&wait_stage, "progress", started.elapsed(), &summary);
                    next_heartbeat = now + HEARTBEAT_INTERVAL;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        Ok(summary)
    }
    .await;
    stage_end(&wait_stage, wait_started, result.is_ok());
    result
}

/// 写入纯测试远端文本，并始终显式使用 CreateNew 策略建立不存在的 fixture 文件。
async fn write_remote_text(
    fixture: &SftpAcceptanceFixture,
    parent: &RemotePath,
    name: &str,
    content: &str,
) -> AppResult<()> {
    fixture
        .files
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: remote_child(&fixture.host_id, parent, name)?,
                content: content.to_owned(),
                strategy: WriteTextStrategy::CreateNew,
            },
        )
        .await?;
    Ok(())
}

/// 递归清理仅由本测试创建的目录；遇到特殊文件时停止而不越权跟随或删除。
async fn remove_remote_tree(files: &FileService, endpoint: FileEndpoint) -> AppResult<()> {
    let mut pending = vec![(endpoint, false)];
    while let Some((directory, visited)) = pending.pop() {
        if visited {
            files
                .delete(
                    CancellationToken::new(),
                    directory,
                    DeleteTargetKind::Directory,
                )
                .await?;
            continue;
        }
        let listing = files
            .list_directory(CancellationToken::new(), directory.clone())
            .await?;
        pending.push((directory, true));
        for entry in listing.entries {
            match entry.kind {
                FileEntryKind::Directory => pending.push((entry.endpoint, false)),
                FileEntryKind::File | FileEntryKind::Symlink => {
                    files
                        .delete(
                            CancellationToken::new(),
                            entry.endpoint,
                            DeleteTargetKind::File,
                        )
                        .await?;
                }
                FileEntryKind::Other => {
                    return Err(AppError::unsupported(
                        "test cleanup found an unsupported remote entry",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 构造强制通过类型化 FileService 的普通文件传输请求，不包含任何凭据字段。
fn request(
    source: FileEndpoint,
    destination: FileEndpoint,
    kind: TransferKind,
    conflict_policy: ConflictPolicy,
) -> TransferRequest {
    TransferRequest {
        source,
        destination,
        kind,
        conflict_policy,
        idle_timeout_seconds: Some(180),
    }
}

/// 把经过域层校验的 POSIX 路径绑定到固定测试主机。
fn remote_endpoint(host_id: &HostId, path: &RemotePath) -> FileEndpoint {
    FileEndpoint::Remote {
        host_id: host_id.clone(),
        path: path.clone(),
    }
}

/// 只通过 RemotePath::join_name 添加单个名称，避免字符串拼接逃出随机测试目录。
fn remote_child(host_id: &HostId, parent: &RemotePath, name: &str) -> AppResult<FileEndpoint> {
    let child = parent
        .join_name(name)
        .map_err(|_| AppError::invalid("invalid isolated SFTP child name"))?;
    Ok(remote_endpoint(host_id, &child))
}

/// 给已经是远端端点的父路径提供同一个 host，避免将 host 标识编码进路径字符串。
fn destination_path(endpoint: &FileEndpoint) -> AppResult<RemotePath> {
    match endpoint {
        FileEndpoint::Remote { path, .. } => Ok(path.clone()),
        _ => Err(AppError::invalid("expected a remote SFTP endpoint")),
    }
}

/// 使用短固定错误说明汇总验收断言，避免失败时误输出 fixture 路径或密钥材料。
fn require(condition: bool, message: &'static str) -> AcceptanceResult {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message).into())
    }
}

/// 读取必需路径但不把环境变量内容放进错误信息。
fn required_path(name: &str) -> AcceptanceResult<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("required task-private fixture path {name} is missing"),
        )
        .into()
    })
}
