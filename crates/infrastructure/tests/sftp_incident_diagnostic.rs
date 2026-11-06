//! 后台事故诊断入口：复用 GPUI 保存主机、vault 与真实 SSH adapter，不启动桌面或 MCP。
//!
//! 必填 KERMINAL_INCIDENT_CONFIG_ROOT、KERMINAL_INCIDENT_HOST_ID、KERMINAL_INCIDENT_COMMAND。
//! 配置根必须是已准备好的隔离副本；只读 exec 的命令由调用者显式提供。
//! 上传另选 saved_host_upload_probe，并显式提供 KERMINAL_INCIDENT_UPLOAD_SOURCE 和
//! KERMINAL_INCIDENT_UPLOAD_REMOTE_DIRECTORY=/tmp/kerminal-incident-<任务唯一后缀>。
//! 可选 KERMINAL_INCIDENT_PAUSE_AFTER_BYTES，在每个指定字节区间暂停并原位继续，共两次。
//! 目录验收另选 saved_host_directory_upload_probe，自动生成隔离源目录，远端目录要求相同。
//! 可选 KERMINAL_INCIDENT_DIRECTORY_TIMEOUT_SECONDS 控制上传期限（默认 900，上限 3600）。
//! 可选 KERMINAL_INCIDENT_PROXY_URL=socks5h://127.0.0.1:49371，仅覆盖内存中的代理。
//! 可选 KERMINAL_INCIDENT_TIMEOUT_SECONDS（默认 60，上限 300），输出总量上限 4 MiB。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CommandEvent, ConnectionIntent, HostConnectionRepository, RemoteConnectionLease,
        RemoteConnectionService, ShellScriptRequest, management::HostManagementService,
    },
    files::FileService,
    preferences::SettingsRepository,
    transfers::{SftpTransferPolicyStore, TransferQueueOptions, TransferService},
};
use kerminal_domain::{
    connections::{HostConnectionProfile, HostEndpoint, ProxyProfile, ProxyProtocol},
    files::{
        ConflictPolicy, FileEndpoint, FileEntryKind, TransferKind, TransferRequest, TransferStatus,
    },
    identity::HostId,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore,
    connections::{
        KnownHostsStore, RusshRemoteConnectionDependencies, RusshRemoteConnectionService,
        management::HostConfigurationRepository,
    },
    files::{
        JsonFileBookmarkRepository, JsonTransferRecoveryRepository, LocalFileProvider,
        SftpFileProvider, SftpTransferBackend,
    },
    vault::VaultStore,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

struct IncidentHostRepository {
    profile: HostConnectionProfile,
    loads: Arc<AtomicUsize>,
}

#[async_trait]
impl HostConnectionRepository for IncidentHostRepository {
    /// 单目标快照仅覆盖代理，保留保存主机的 endpoint、认证和跳板，禁止猜测其它目标。
    /// 每次 load 对应本 harness 的新连接尝试，计数不能解读为认证成功次数。
    ///
    /// @author kongweiguang
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        if host_id != &self.profile.host_id {
            return Err(AppError::not_found("事故诊断没有该主机快照"));
        }
        let count = self.loads.fetch_add(1, Ordering::AcqRel) + 1;
        eprintln!("[incident] event=ssh-host-load fresh_connect_attempts={count}");
        Ok(self.profile.clone())
    }
}

/// 错误只保留公开安全字段，防止测试框架的 anyhow source 链打印协议或凭据内部状态。
///
/// @author kongweiguang
fn safe_error(error: AppError) -> anyhow::Error {
    anyhow!(
        "{}: {}",
        error.public_code().as_str(),
        error.public_message()
    )
}

/// 隔离根必须预先存在，且不得落在当前用户任一真实 Kerminal 数据根内部。
/// 一次性私钥路径夹具没有 vault；真实凭据引用仍由 VaultStore 按自身格式严格读取。
///
/// @author kongweiguang
fn isolated_root() -> Result<PathBuf> {
    let supplied = PathBuf::from(
        std::env::var_os("KERMINAL_INCIDENT_CONFIG_ROOT")
            .context("missing KERMINAL_INCIDENT_CONFIG_ROOT")?,
    );
    if !supplied.is_absolute() {
        bail!("incident config root must be absolute");
    }
    let root = supplied
        .canonicalize()
        .context("incident config root must exist")?;
    if !root.is_dir() {
        bail!("incident config root must be a directory");
    }
    let user_directory = dirs::home_dir().context("cannot locate user directory")?;
    for name in [".kerminal", ".kerminal-gpui"] {
        if let Ok(real_root) = user_directory.join(name).canonicalize()
            && (root.starts_with(&real_root) || real_root.starts_with(&root))
        {
            bail!("incident config root must be isolated from live Kerminal data");
        }
    }
    if !root.join("known_hosts").is_file() {
        bail!("isolated config is missing known_hosts");
    }
    Ok(root)
}

/// 仅接受无认证的 loopback SOCKS5 地址；目标域名由 SOCKS 协议传递，支持 socks5h 语义。
///
/// @author kongweiguang
fn proxy_override() -> Result<Option<ProxyProfile>> {
    let Ok(value) = std::env::var("KERMINAL_INCIDENT_PROXY_URL") else {
        return Ok(None);
    };
    let address = value
        .strip_prefix("socks5h://")
        .or_else(|| value.strip_prefix("socks5://"))
        .context("incident proxy must use socks5h:// or socks5://")?;
    let address: SocketAddr = address
        .parse()
        .context("incident proxy must be an IP:port")?;
    if !address.ip().is_loopback() || address.port() == 0 {
        bail!("incident proxy must be a loopback listener with a nonzero port");
    }
    Ok(Some(ProxyProfile {
        protocol: ProxyProtocol::Socks5,
        endpoint: HostEndpoint {
            host: address.ip().to_string(),
            port: address.port(),
            username: String::new(),
        },
        password: None,
    }))
}

/// reader 有总量和期限上限；不回显命令、主机配置或凭据，失败后仍显式关闭原 channel。
///
/// @author kongweiguang
async fn run_command(
    lease: &RemoteConnectionLease,
    command: String,
    cancellation: &CancellationToken,
    operation_timeout: Duration,
) -> Result<()> {
    let mut stream = timeout(
        operation_timeout,
        lease.exec_shell(
            ShellScriptRequest {
                script: command,
                environment: BTreeMap::new(),
            },
            cancellation.clone(),
        ),
    )
    .await
    .context("incident exec open timed out")?
    .map_err(safe_error)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let read = timeout(operation_timeout, async {
        while let Some(event) = stream.next_event().await.map_err(safe_error)? {
            match event {
                CommandEvent::Stdout(bytes) => {
                    if stdout.len() + stderr.len() + bytes.len() > OUTPUT_LIMIT {
                        bail!("incident output exceeded 4 MiB");
                    }
                    stdout.extend(bytes);
                }
                CommandEvent::Stderr(bytes) => {
                    if stdout.len() + stderr.len() + bytes.len() > OUTPUT_LIMIT {
                        bail!("incident output exceeded 4 MiB");
                    }
                    stderr.extend(bytes);
                }
                CommandEvent::Exited { code, signal } => {
                    eprintln!(
                        "[incident] event=exit code={code:?} signalled={}",
                        signal.is_some()
                    );
                    if signal.is_some() || code != Some(0) {
                        bail!("incident command did not exit successfully");
                    }
                    return Ok(());
                }
            }
        }
        bail!("incident exec ended without an exit-status");
    })
    .await
    .context("incident exec read timed out")
    .and_then(|result| result);
    if read.is_err() {
        cancellation.cancel();
    }
    let close = stream
        .close_until(Instant::now() + Duration::from_secs(10))
        .await;
    println!(
        "[incident] stdout-begin\n{}\n[incident] stdout-end",
        String::from_utf8_lossy(&stdout)
    );
    eprintln!(
        "[incident] stderr-begin\n{}\n[incident] stderr-end",
        String::from_utf8_lossy(&stderr)
    );
    if let Err(error) = close {
        eprintln!(
            "[incident] event=channel-close-failed code={}",
            error.public_code().as_str()
        );
        read?;
        return Err(safe_error(error));
    }
    read
}

/// 仅显式 ignored 调用才联网；配置初始化、vault 解析和 known_hosts 都绑定隔离副本。
/// 操作、lease 和 service 分层收尾，失败不把尚未回收的连接报告为成功。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit isolated GPUI config copy, host ID and read-only incident command"]
async fn saved_host_read_only_exec() -> Result<()> {
    let root = isolated_root()?;
    let host_text =
        std::env::var("KERMINAL_INCIDENT_HOST_ID").context("missing incident host ID")?;
    let host_id = HostId::parse(&host_text).map_err(|_| anyhow!("invalid incident host ID"))?;
    let command = std::env::var("KERMINAL_INCIDENT_COMMAND").context("missing incident command")?;
    if command.trim().is_empty() || command.len() > 64 * 1024 {
        bail!("incident command must contain 1..65536 bytes");
    }
    let seconds = std::env::var("KERMINAL_INCIDENT_TIMEOUT_SECONDS")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(60))
        .context("invalid incident timeout")?;
    if !(1..=300).contains(&seconds) {
        bail!("incident timeout must be within 1..300 seconds");
    }
    let operation_timeout = Duration::from_secs(seconds);
    let configuration = ConfigurationStore::initialize(&root)
        .map_err(|_| anyhow!("isolated configuration initialization failed"))?;
    let storage = configuration.storage_coordinator();
    let saved_hosts = HostConfigurationRepository::new(configuration);
    let mut profile = saved_hosts.load(&host_id).await.map_err(safe_error)?;
    let proxy = proxy_override()?;
    let overridden = proxy.is_some();
    if let Some(proxy) = proxy {
        profile.route.proxy = Some(proxy);
    }
    let connections = Arc::new(RusshRemoteConnectionService::new(
        RusshRemoteConnectionDependencies {
            hosts: Arc::new(IncidentHostRepository {
                profile,
                loads: Arc::new(AtomicUsize::new(0)),
            }),
            credentials: Arc::new(VaultStore::open(storage.clone())),
            known_hosts: Arc::new(KnownHostsStore::new(storage, false)),
        },
    ));
    let cancellation = CancellationToken::new();
    eprintln!("[incident] event=connect-start proxy_override={overridden}");
    let connected = timeout(
        operation_timeout,
        connections.connect(host_id, ConnectionIntent::Exec, cancellation.clone()),
    )
    .await
    .context("incident SSH connect timed out")
    .and_then(|result| result.map_err(safe_error));
    let result = match connected {
        Ok(lease) => {
            eprintln!("[incident] event=connected");
            let command_result =
                run_command(&lease, command, &cancellation, operation_timeout).await;
            cancellation.cancel();
            let close = lease
                .close_until(
                    Instant::now() + Duration::from_secs(10),
                    "incident finished",
                )
                .await;
            if let Err(error) = close {
                eprintln!(
                    "[incident] event=lease-close-failed code={}",
                    error.public_code().as_str()
                );
                command_result.and(Err(safe_error(error)))
            } else {
                command_result
            }
        }
        Err(error) => Err(error),
    };
    cancellation.cancel();
    let shutdown = connections
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await;
    if let Err(error) = shutdown {
        eprintln!(
            "[incident] event=service-close-failed code={}",
            error.public_code().as_str()
        );
        return result.and(Err(safe_error(error)));
    }
    result
}

struct UploadEvidence {
    file: Mutex<std::fs::File>,
}

impl UploadEvidence {
    /// 每个阶段立即追加证据，测试失败或清理失败后仍能回读；不记录认证引用或主机文档。
    ///
    /// @author kongweiguang
    fn record(&self, mut event: Value) -> Result<()> {
        event["unix_ms"] = json!(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis());
        let line = serde_json::to_string(&event)?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow!("incident evidence lock poisoned"))?;
        writeln!(file, "{line}")?;
        file.flush()?;
        eprintln!("[incident-upload] {line}");
        Ok(())
    }
}

/// 单文件探针只读取显式指定的常规文件，避免把目录误当作续传样本。
///
/// @author kongweiguang
fn upload_inputs() -> Result<(PathBuf, String)> {
    let source = PathBuf::from(
        std::env::var_os("KERMINAL_INCIDENT_UPLOAD_SOURCE")
            .context("missing KERMINAL_INCIDENT_UPLOAD_SOURCE")?,
    );
    if !source.is_absolute() {
        bail!("incident upload source must be absolute");
    }
    let source = source
        .canonicalize()
        .context("incident upload source must exist")?;
    if !source.is_file() {
        bail!("incident upload source must be a regular file");
    }
    Ok((source, upload_remote_directory()?))
}

/// 只接受 /tmp 的单个私有目录组件，供两种探针安全复用 shell 命令。
///
/// @author kongweiguang
fn upload_remote_directory() -> Result<String> {
    let directory = std::env::var("KERMINAL_INCIDENT_UPLOAD_REMOTE_DIRECTORY")
        .context("missing KERMINAL_INCIDENT_UPLOAD_REMOTE_DIRECTORY")?;
    let suffix = directory
        .strip_prefix("/tmp/kerminal-incident-")
        .context("incident upload directory must start with /tmp/kerminal-incident-")?;
    if suffix.is_empty()
        || suffix.len() > 100
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("incident upload directory must be one private /tmp component");
    }
    Ok(directory)
}

/// 独立读取源摘要作为端到端核对基准；不改源文件，不把本机哈希字节算成已上传进度。
///
/// @author kongweiguang
async fn source_digest(source: &std::path::Path) -> Result<(u64, String)> {
    let mut file = tokio::fs::File::open(source).await?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

/// 小型控制命令保留短读取期限，避免心跳和单文件探针受目录校验预算影响。
///
/// @author kongweiguang
async fn capture_upload_command(lease: &RemoteConnectionLease, script: String) -> Result<Vec<u8>> {
    capture_upload_command_with_timeout(lease, script, Duration::from_secs(12)).await
}

/// 在原 lease 打开 channel，并按调用方提供的读取期限等待真实 exit-status 和 CLOSE。
///
/// @author kongweiguang
async fn capture_upload_command_with_timeout(
    lease: &RemoteConnectionLease,
    script: String,
    read_timeout: Duration,
) -> Result<Vec<u8>> {
    let cancellation = CancellationToken::new();
    let opened = timeout(
        Duration::from_secs(8),
        lease.exec_shell(
            ShellScriptRequest {
                script,
                environment: BTreeMap::new(),
            },
            cancellation.clone(),
        ),
    )
    .await;
    let mut stream = match opened {
        Ok(result) => result.map_err(safe_error)?,
        Err(_) => {
            cancellation.cancel();
            bail!("upload probe exec open timed out");
        }
    };
    let mut output = Vec::new();
    let read = timeout(read_timeout, async {
        while let Some(event) = stream.next_event().await.map_err(safe_error)? {
            match event {
                CommandEvent::Stdout(bytes) => {
                    if output.len() + bytes.len() > 64 * 1024 {
                        bail!("upload probe output too large");
                    }
                    output.extend(bytes);
                }
                CommandEvent::Stderr(bytes) => {
                    if bytes.len() > 64 * 1024 {
                        bail!("upload probe stderr too large");
                    }
                }
                CommandEvent::Exited {
                    code: Some(0),
                    signal: None,
                } => return Ok(()),
                CommandEvent::Exited { code, signal } => bail!(
                    "upload probe command failed: exit={code:?}, signalled={}",
                    signal.is_some()
                ),
            }
        }
        bail!("upload probe command had no exit-status");
    })
    .await
    .context("upload probe exec read timed out")
    .and_then(|result| result);
    if read.is_err() {
        cancellation.cancel();
    }
    let close = stream
        .close_until(Instant::now() + Duration::from_secs(5))
        .await;
    if let Err(error) = close {
        eprintln!(
            "[incident-upload] event=exec-close-failed code={}",
            error.public_code().as_str()
        );
        read?;
        return Err(safe_error(error));
    }
    read?;
    Ok(output)
}

/// 心跳只使用独立 lease 的 exec channel，每两秒确认一次往返，不通过反复 SSH 登录制造负载。
///
/// @author kongweiguang
async fn upload_heartbeat(
    lease: RemoteConnectionLease,
    stop: CancellationToken,
    evidence: Arc<UploadEvidence>,
    loads: Arc<AtomicUsize>,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        let started = Instant::now();
        let result =
            capture_upload_command(&lease, "printf 'kerminal-incident-heartbeat\\n'".to_owned())
                .await;
        let okay = result
            .as_ref()
            .is_ok_and(|bytes| bytes == b"kerminal-incident-heartbeat\n");
        evidence.record(json!({"event":"heartbeat", "ok":okay,
            "elapsed_ms":started.elapsed().as_millis(), "fresh_connect_attempts":loads.load(Ordering::Acquire)}))?;
        result?;
        if !okay {
            bail!("upload heartbeat response mismatch");
        }
    }
}

/// 只有调用方创建成功的目录且随机 owner 仍匹配才删除精确 payload，rmdir 拒绝遗留的其它项目。
/// marker 未写成功时仅尝试空目录删除；不递归清理、不删除引擎未知 staging 或其它文件。
///
/// @author kongweiguang
async fn cleanup_upload_directory(
    lease: &RemoteConnectionLease,
    directory: &str,
    owner: &str,
) -> Result<()> {
    capture_upload_command(lease, format!(
        "test ! -L '{directory}' && test -d '{directory}' || exit 71; \
         if test -f '{directory}/.incident-owner' && test ! -L '{directory}/.incident-owner' \
         && test \"$(cat '{directory}/.incident-owner')\" = '{owner}'; then \
         rm -f -- '{directory}/payload.bin' && rm -- '{directory}/.incident-owner' || exit 72; \
         elif test -e '{directory}/.incident-owner' || test -L '{directory}/.incident-owner'; then exit 73; fi; \
         rmdir -- '{directory}'"
    )).await?;
    Ok(())
}

/// 显式上传仅写新建私有 /tmp 目录；真实队列和文件服务共享 GPUI 设置、vault 与 SSH adapter。
/// 120 秒总传输期限到达即取消并 join；清理失败留本地 JSONL、恢复事实及远端 owner 供核对。
/// 上传后下载回本轮隔离根，两向都用可选的两次暂停核对原位恢复和首个新写入耗时。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated GPUI config plus explicit source and new private /tmp upload directory"]
async fn saved_host_upload_probe() -> Result<()> {
    let root = isolated_root()?;
    let (source, directory) = upload_inputs()?;
    let pause_interval = std::env::var("KERMINAL_INCIDENT_PAUSE_AFTER_BYTES")
        .ok()
        .map(|value| value.parse::<u64>().context("invalid pause byte interval"))
        .transpose()?;
    if pause_interval == Some(0) {
        bail!("pause byte interval must be positive");
    }
    let host_text =
        std::env::var("KERMINAL_INCIDENT_HOST_ID").context("missing incident host ID")?;
    let host_id = HostId::parse(&host_text).map_err(|_| anyhow!("invalid incident host ID"))?;
    let owner = uuid::Uuid::new_v4().simple().to_string();
    let evidence_path = root.join(format!("incident-upload-{owner}.jsonl"));
    let evidence = Arc::new(UploadEvidence {
        file: Mutex::new(
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&evidence_path)?,
        ),
    });
    eprintln!("[incident-upload] evidence={}", evidence_path.display());
    let (expected_size, expected_sha256) = source_digest(&source).await?;
    evidence.record(
        json!({"event":"source-hashed", "bytes":expected_size, "sha256":expected_sha256,
        "remote_directory":directory, "owner":owner}),
    )?;
    let configuration = ConfigurationStore::initialize(&root)
        .map_err(|_| anyhow!("isolated configuration initialization failed"))?;
    let policy = configuration
        .load_settings()
        .await
        .map_err(safe_error)?
        .settings
        .sftp;
    let storage = configuration.storage_coordinator();
    let saved_hosts = Arc::new(HostConfigurationRepository::new(configuration));
    let mut profile = saved_hosts.load(&host_id).await.map_err(safe_error)?;
    if let Some(proxy) = proxy_override()? {
        profile.route.proxy = Some(proxy);
    }
    let loads = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(RusshRemoteConnectionService::new(
        RusshRemoteConnectionDependencies {
            hosts: Arc::new(IncidentHostRepository {
                profile,
                loads: loads.clone(),
            }),
            credentials: Arc::new(VaultStore::open(storage.clone())),
            known_hosts: Arc::new(KnownHostsStore::new(storage.clone(), false)),
        },
    ));
    let remote: Arc<dyn RemoteConnectionService> = connections.clone();
    let files = FileService::new(
        vec![
            Arc::new(LocalFileProvider),
            Arc::new(SftpFileProvider::new(remote.clone())),
        ],
        Arc::new(JsonFileBookmarkRepository::new(root.clone())),
    )
    .map_err(safe_error)?;
    let transfers = TransferService::new_with_recovery_repository(
        vec![Arc::new(SftpTransferBackend::new(remote))],
        tokio::runtime::Handle::current(),
        TransferQueueOptions::default(),
        SftpTransferPolicyStore::new(policy).map_err(safe_error)?,
        Arc::new(JsonTransferRecoveryRepository::with_storage_coordinator(
            storage,
        )),
        Some(Arc::new(HostManagementService::new(saved_hosts))),
    )
    .map_err(safe_error)?;
    let directory_endpoint = FileEndpoint::remote(host_id.clone(), &directory)
        .map_err(|_| anyhow!("invalid incident remote directory"))?;
    let destination = FileEndpoint::remote(host_id.clone(), &format!("{directory}/payload.bin"))
        .map_err(|_| anyhow!("invalid incident remote payload"))?;
    let connection_cancel = CancellationToken::new();
    let mut created_directory = false;
    let mut lease: Option<RemoteConnectionLease> = None;
    let mut transfer_id = None;
    let stop_heartbeat = CancellationToken::new();
    let mut heartbeat = None;
    let exercise = async {
        let opened = timeout(Duration::from_secs(20), connections.connect(host_id,
            ConnectionIntent::Exec, connection_cancel.clone())).await
            .context("upload probe SSH connect timed out")?.map_err(safe_error)?;
        lease = Some(opened);
        let probe_lease = lease.as_ref().context("upload probe lease missing")?;
        // create_directory 是独占单层创建；已有目录永不收养，失败时不获得删除资格。
        let creation_cancel = CancellationToken::new();
        let created = timeout(Duration::from_secs(20), files.create_directory(
            creation_cancel.clone(), directory_endpoint.clone())).await;
        if created.is_err() { creation_cancel.cancel(); }
        created.context("incident directory creation timed out")?.map_err(safe_error)?;
        created_directory = true;
        capture_upload_command(probe_lease, format!(
            "umask 077; (set -C; printf '%s' '{owner}' > '{directory}/.incident-owner')"
        )).await?;
        evidence.record(json!({"event":"directory-owned", "fresh_connect_attempts":loads.load(Ordering::Acquire)}))?;
        heartbeat = Some(tokio::spawn(upload_heartbeat(probe_lease.clone(), stop_heartbeat.clone(),
            evidence.clone(), loads.clone())));
        let downloaded = root.join(format!("incident-download-{owner}.bin"));
        for (direction, request) in [
            ("upload", TransferRequest { source: FileEndpoint::Local { path: source.clone() },
                destination: destination.clone(), kind: TransferKind::File,
                conflict_policy: ConflictPolicy::Rename, idle_timeout_seconds: Some(120) }),
            ("download", TransferRequest { source: destination.clone(),
                destination: FileEndpoint::Local { path: downloaded.clone() }, kind: TransferKind::File,
                conflict_policy: ConflictPolicy::Rename, idle_timeout_seconds: Some(120) }),
        ] {
        let expected_destination = request.destination.clone();
        let started = Instant::now();
        let upload = timeout(Duration::from_secs(120), async {
            let initial = transfers.enqueue(request).await.map_err(safe_error)?;
            transfer_id = Some(initial.id.clone());
            let mut next_report = Instant::now();
            let mut pauses = 0_u64;
            let mut resume_started: Option<(Instant, u64)> = None;
            loop {
                let summary = transfers.get(&initial.id).await.map_err(safe_error)?;
                if summary.phase == Some(kerminal_application::transfers::TransferPhase::Verifying) {
                    bail!("ordinary transfer/resume unexpectedly entered prefix verification");
                }
                if let Some((resumed_at, offset)) = resume_started
                    && summary.phase == Some(kerminal_application::transfers::TransferPhase::Transferring)
                    && summary.bytes_transferred > offset
                {
                        evidence.record(json!({"event":"resume-writing", "direction":direction, "resume_number":pauses,
                            "resume_to_write_ms":resumed_at.elapsed().as_millis(),
                            "confirmed_bytes":summary.bytes_transferred, "same_id":summary.id == initial.id}))?;
                        resume_started = None;
                }
                if pauses < 2 && pause_interval.is_some_and(|interval| {
                    summary.phase == Some(kerminal_application::transfers::TransferPhase::Transferring)
                        && summary.bytes_transferred >= interval.saturating_mul(pauses + 1)
                        && summary.bytes_transferred < expected_size
                }) {
                    transfers.cancel(&initial.id).await.map_err(safe_error)?;
                    let paused = timeout(Duration::from_secs(15), async {
                        loop {
                            if let Some(paused) = transfers.get_settled(&initial.id).await.map_err(safe_error)? {
                                break Ok::<_, anyhow::Error>(paused);
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.context("paused transfer did not settle")??;
                    if paused.status != TransferStatus::Cancelled || !paused.resumable || paused.cleanup_pending {
                        bail!("pause did not retain a usable checkpoint with released resources");
                    }
                    evidence.record(json!({"event":"paused", "direction":direction, "pause_number":pauses + 1,
                        "id":paused.id.to_string(), "confirmed_bytes":paused.bytes_transferred}))?;
                    let resumed_at = Instant::now();
                    let resumed = transfers.resume(&initial.id).await.map_err(safe_error)?;
                    let rows = transfers.list().await;
                    if resumed.id != initial.id || rows.len() != if direction == "upload" { 1 } else { 2 }
                        || rows.iter().filter(|row| row.id == initial.id).count() != 1 {
                        bail!("resume changed the task identity or created an extra queue record");
                    }
                    pauses += 1;
                    resume_started = Some((resumed_at, resumed.bytes_transferred));
                    evidence.record(json!({"event":"resumed", "direction":direction, "resume_number":pauses,
                        "id":resumed.id.to_string(), "queue_rows":rows.len(),
                        "confirmed_bytes":resumed.bytes_transferred}))?;
                    continue;
                }
                if Instant::now() >= next_report {
                    evidence.record(json!({"event":"progress", "direction":direction, "elapsed_ms":started.elapsed().as_millis(),
                        "status":format!("{:?}", summary.status), "phase":format!("{:?}", summary.phase),
                        "confirmed_bytes":summary.bytes_transferred, "total_bytes":summary.total_bytes,
                        "cleanup_pending":summary.cleanup_pending, "fresh_connect_attempts":loads.load(Ordering::Acquire)}))?;
                    next_report = Instant::now() + Duration::from_secs(2);
                }
                if let Some(summary) = transfers.get_settled(&initial.id).await.map_err(safe_error)? {
                    evidence.record(json!({"event":"transfer-settled", "direction":direction, "status":format!("{:?}",summary.status),
                        "confirmed_bytes":summary.bytes_transferred, "cleanup_pending":summary.cleanup_pending,
                        "failure_kind":format!("{:?}",summary.failure_kind)}))?;
                    if summary.status != TransferStatus::Succeeded || summary.actual_destination.as_ref() != Some(&expected_destination) {
                        bail!("incident upload did not succeed at the exact reserved destination");
                    }
                    if pause_interval.is_some() && (pauses != 2 || resume_started.is_some()) {
                        bail!("transfer completed without observing both requested resumes writing new bytes");
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok::<(), anyhow::Error>(())
        }).await.context("incident upload exceeded 120 seconds").and_then(|result| result);
        if upload.is_err() && let Some(id) = &transfer_id {
            transfers.cancel(id).await.map_err(safe_error)?;
        }
        upload?;
        }
        let (downloaded_size, downloaded_digest) = source_digest(&downloaded).await?;
        if downloaded_size != expected_size || downloaded_digest != expected_sha256 {
            bail!("resumed download size or SHA-256 mismatch");
        }
        evidence.record(json!({"event":"download-verified", "bytes":downloaded_size,
            "observed_sha256":downloaded_digest}))?;
        let listing = timeout(Duration::from_secs(20), files.list_directory(
            CancellationToken::new(), directory_endpoint.clone())).await.context("incident size read timed out")?.map_err(safe_error)?;
        let entry = listing.entries.iter().find(|entry| entry.endpoint == destination)
            .context("incident uploaded file missing from SFTP listing")?;
        if entry.kind != FileEntryKind::File || entry.size != Some(expected_size) {
            bail!("incident remote file kind or size mismatch");
        }
        let digest = capture_upload_command(probe_lease,
            format!("sha256sum -- '{directory}/payload.bin'" )).await?;
        let observed = std::str::from_utf8(&digest)?.split_whitespace().next().context("remote digest missing")?;
        evidence.record(json!({"event":"verified", "bytes":entry.size, "expected_sha256":expected_sha256,
            "observed_sha256":observed, "fresh_connect_attempts":loads.load(Ordering::Acquire)}))?;
        if observed != expected_sha256 { bail!("incident remote SHA-256 mismatch"); }
        Ok::<(), anyhow::Error>(())
    }.await;
    let mut failures = Vec::new();
    if let Err(error) = evidence.record(json!({"event":"exercise-finished", "ok":exercise.is_ok(),
        "error":exercise.as_ref().err().map(ToString::to_string)}))
    {
        failures.push(format!("evidence write: {error}"));
    }
    stop_heartbeat.cancel();
    if let Some(worker) = heartbeat {
        match worker.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("heartbeat: {error}")),
            Err(_) => failures.push("heartbeat task failed".to_owned()),
        }
    }
    if let Some(id) = &transfer_id
        && let Err(error) = transfers.cancel(id).await
    {
        failures.push(format!("cancel: {}", safe_error(error)));
    }
    let queue_closed = transfers
        .shutdown_until(Instant::now() + Duration::from_secs(20))
        .await;
    let can_clean = queue_closed.is_ok();
    if let Err(error) = queue_closed {
        failures.push(format!("queue shutdown: {}", safe_error(error)));
    }
    if created_directory && can_clean {
        if let Some(probe_lease) = &lease
            && let Err(error) = cleanup_upload_directory(probe_lease, &directory, &owner).await
        {
            failures.push(format!("owned remote cleanup: {error}"));
        }
    } else if created_directory {
        failures.push(
            "remote owner retained because worker/resource shutdown is unconfirmed".to_owned(),
        );
    }
    connection_cancel.cancel();
    if let Some(probe_lease) = lease
        && let Err(error) = probe_lease
            .close_until(
                Instant::now() + Duration::from_secs(10),
                "incident upload finished",
            )
            .await
    {
        failures.push(format!("probe lease close: {}", safe_error(error)));
    }
    if let Err(error) = connections
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await
    {
        failures.push(format!("SSH service shutdown: {}", safe_error(error)));
    }
    evidence.record(
        json!({"event":"cleanup-finished", "ok":failures.is_empty(), "failures":failures,
        "fresh_connect_attempts":loads.load(Ordering::Acquire)}),
    )?;
    exercise?;
    if !failures.is_empty() {
        bail!("incident probe cleanup/heartbeat failed; see retained JSONL evidence");
    }
    Ok(())
}

const DIRECTORY_PROBE_FILES: usize = 1057;

/// 夹具名称按字节排序与远端 C locale 的 glob 顺序一致，使摘要同时覆盖名称和内容。
///
/// @author kongweiguang
fn directory_probe_fixture() -> Result<(tempfile::TempDir, String)> {
    let source = tempfile::tempdir().context("create isolated directory fixture")?;
    let mut manifest = Sha256::new();
    for index in 0..DIRECTORY_PROBE_FILES {
        let name = format!("item-{index:04}.bin");
        let content = format!("kerminal-directory-fixture-{index:04}\n");
        std::fs::write(source.path().join(&name), content.as_bytes())?;
        let digest = Sha256::digest(content.as_bytes());
        let entry = format!("{digest:x}  {name}\n");
        manifest.update(entry.as_bytes());
    }
    Ok((source, format!("{:x}", manifest.finalize())))
}

/// 仅删除新建私有目录中确定属于本夹具的名称；任何额外条目都会保留 owner 并使清理失败。
///
/// @author kongweiguang
async fn cleanup_directory_probe(
    lease: &RemoteConnectionLease,
    directory: &str,
    owner: &str,
) -> Result<()> {
    let names = (0..DIRECTORY_PROBE_FILES)
        .map(|index| format!("item-{index:04}.bin"))
        .collect::<Vec<_>>()
        .join(" ");
    capture_upload_command_with_timeout(
        lease,
        format!(
            "test ! -L '{directory}' && test -d '{directory}' || exit 71; \
             test ! -L '{directory}/.incident-owner' && test -f '{directory}/.incident-owner' \
             && test \"$(cat '{directory}/.incident-owner')\" = '{owner}' || exit 72; \
             test ! -L '{directory}/payload' && test -d '{directory}/payload' || exit 73; \
             (cd -- '{directory}/payload' && rm -f -- {names}) || exit 74; \
             rmdir -- '{directory}/payload' || exit 75; \
             test -z \"$(find '{directory}' -mindepth 1 -maxdepth 1 ! -name .incident-owner -print -quit)\" || exit 76; \
             rm -- '{directory}/.incident-owner' && rmdir -- '{directory}'"
        ),
        Duration::from_secs(120),
    )
    .await?;
    Ok(())
}

/// 使用保存主机和真实队列验证超过服务端常见句柄上限的目录上传，失败时保留私有现场。
/// 成功必须核对全部文件名、数量和内容，并在队列关闭后按 owner 回收夹具。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated GPUI config and a new private /tmp upload directory"]
async fn saved_host_directory_upload_probe() -> Result<()> {
    let root = isolated_root()?;
    let directory = upload_remote_directory()?;
    let seconds = std::env::var("KERMINAL_INCIDENT_DIRECTORY_TIMEOUT_SECONDS")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(900))
        .context("invalid directory upload timeout")?;
    if !(1..=3600).contains(&seconds) {
        bail!("directory upload timeout must be within 1..3600 seconds");
    }
    let host_text =
        std::env::var("KERMINAL_INCIDENT_HOST_ID").context("missing incident host ID")?;
    let host_id = HostId::parse(&host_text).map_err(|_| anyhow!("invalid incident host ID"))?;
    let (source, expected_manifest) = directory_probe_fixture()?;
    let owner = uuid::Uuid::new_v4().simple().to_string();
    let evidence_path = root.join(format!("incident-directory-upload-{owner}.jsonl"));
    let evidence = UploadEvidence {
        file: Mutex::new(
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&evidence_path)?,
        ),
    };
    eprintln!("[incident-directory] evidence={}", evidence_path.display());
    evidence.record(
        json!({"event":"source-created", "files":DIRECTORY_PROBE_FILES,
        "manifest_sha256":expected_manifest, "remote_directory":directory, "owner":owner}),
    )?;

    let configuration = ConfigurationStore::initialize(&root)
        .map_err(|_| anyhow!("isolated configuration initialization failed"))?;
    let policy = configuration
        .load_settings()
        .await
        .map_err(safe_error)?
        .settings
        .sftp;
    let storage = configuration.storage_coordinator();
    let saved_hosts = Arc::new(HostConfigurationRepository::new(configuration));
    let mut profile = saved_hosts.load(&host_id).await.map_err(safe_error)?;
    if let Some(proxy) = proxy_override()? {
        profile.route.proxy = Some(proxy);
    }
    let loads = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(RusshRemoteConnectionService::new(
        RusshRemoteConnectionDependencies {
            hosts: Arc::new(IncidentHostRepository {
                profile,
                loads: loads.clone(),
            }),
            credentials: Arc::new(VaultStore::open(storage.clone())),
            known_hosts: Arc::new(KnownHostsStore::new(storage.clone(), false)),
        },
    ));
    let remote: Arc<dyn RemoteConnectionService> = connections.clone();
    let files = FileService::new(
        vec![
            Arc::new(LocalFileProvider),
            Arc::new(SftpFileProvider::new(remote.clone())),
        ],
        Arc::new(JsonFileBookmarkRepository::new(root.clone())),
    )
    .map_err(safe_error)?;
    let transfers = TransferService::new_with_recovery_repository(
        vec![Arc::new(SftpTransferBackend::new(remote))],
        tokio::runtime::Handle::current(),
        TransferQueueOptions::default(),
        SftpTransferPolicyStore::new(policy).map_err(safe_error)?,
        Arc::new(JsonTransferRecoveryRepository::with_storage_coordinator(
            storage,
        )),
        Some(Arc::new(HostManagementService::new(saved_hosts))),
    )
    .map_err(safe_error)?;
    let directory_endpoint = FileEndpoint::remote(host_id.clone(), &directory)
        .map_err(|_| anyhow!("invalid incident remote directory"))?;
    let destination = FileEndpoint::remote(host_id.clone(), &format!("{directory}/payload"))
        .map_err(|_| anyhow!("invalid incident remote payload"))?;
    let connection_cancel = CancellationToken::new();
    let mut created_directory = false;
    let mut lease: Option<RemoteConnectionLease> = None;
    let mut transfer_id = None;
    let exercise = async {
        let opened = timeout(
            Duration::from_secs(20),
            connections.connect(host_id, ConnectionIntent::Exec, connection_cancel.clone()),
        )
        .await
        .context("directory probe SSH connect timed out")?
        .map_err(safe_error)?;
        lease = Some(opened);
        let probe_lease = lease.as_ref().context("directory probe lease missing")?;
        let creation_cancel = CancellationToken::new();
        let created = timeout(
            Duration::from_secs(20),
            files.create_directory(creation_cancel.clone(), directory_endpoint.clone()),
        )
        .await;
        if created.is_err() {
            creation_cancel.cancel();
        }
        created
            .context("directory probe creation timed out")?
            .map_err(safe_error)?;
        created_directory = true;
        capture_upload_command(
            probe_lease,
            format!("umask 077; (set -C; printf '%s' '{owner}' > '{directory}/.incident-owner')"),
        )
        .await?;
        evidence.record(json!({"event":"directory-owned"}))?;

        let request = TransferRequest {
            source: FileEndpoint::Local {
                path: source.path().to_path_buf(),
            },
            destination: destination.clone(),
            kind: TransferKind::Directory,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: Some(120),
        };
        let started = Instant::now();
        timeout(Duration::from_secs(seconds), async {
            let initial = transfers.enqueue(request).await.map_err(safe_error)?;
            transfer_id = Some(initial.id.clone());
            let mut next_report = Instant::now();
            loop {
                if let Some(summary) = transfers.get_settled(&initial.id).await.map_err(safe_error)? {
                    evidence.record(json!({"event":"transfer-settled", "status":format!("{:?}", summary.status),
                        "confirmed_bytes":summary.bytes_transferred, "cleanup_pending":summary.cleanup_pending,
                        "failure_kind":format!("{:?}", summary.failure_kind)}))?;
                    if summary.status != TransferStatus::Succeeded
                        || summary.cleanup_pending
                        || summary.actual_destination.as_ref() != Some(&destination)
                    {
                        bail!("directory upload did not settle successfully at its reserved destination");
                    }
                    break;
                }
                if Instant::now() >= next_report {
                    let summary = transfers.get(&initial.id).await.map_err(safe_error)?;
                    evidence.record(json!({"event":"progress", "elapsed_ms":started.elapsed().as_millis(),
                        "status":format!("{:?}", summary.status), "phase":format!("{:?}", summary.phase),
                        "confirmed_bytes":summary.bytes_transferred, "cleanup_pending":summary.cleanup_pending}))?;
                    next_report = Instant::now() + Duration::from_secs(10);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("directory upload exceeded its deadline")??;

        let verification = capture_upload_command_with_timeout(
            probe_lease,
            format!(
                "set -eu; test ! -L '{directory}/payload'; cd -- '{directory}/payload'; \
                 total=$(find . -mindepth 1 -maxdepth 1 -print | wc -l); \
                 regular=$(find . -mindepth 1 -maxdepth 1 -type f -name 'item-[0-9][0-9][0-9][0-9].bin' -print | wc -l); \
                 test \"$total\" -eq {DIRECTORY_PROBE_FILES} || exit 74; \
                 test \"$regular\" -eq {DIRECTORY_PROBE_FILES} || exit 75; \
                 LC_ALL=C; export LC_ALL; sha256sum -- item-*.bin | sha256sum"
            ),
            Duration::from_secs(120),
        )
        .await?;
        let observed = std::str::from_utf8(&verification)?
            .split_whitespace()
            .next()
            .context("directory manifest digest missing")?;
        evidence.record(json!({"event":"verified", "files":DIRECTORY_PROBE_FILES,
            "expected_manifest_sha256":expected_manifest, "observed_manifest_sha256":observed,
            "fresh_connect_attempts":loads.load(Ordering::Acquire)}))?;
        if observed != expected_manifest {
            bail!("directory upload names or contents do not match the source manifest");
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let mut failures = Vec::new();
    if let Err(error) = evidence.record(json!({"event":"exercise-finished", "ok":exercise.is_ok(),
        "error":exercise.as_ref().err().map(ToString::to_string)}))
    {
        failures.push(format!("evidence write: {error}"));
    }
    if exercise.is_err()
        && let Some(id) = &transfer_id
        && let Err(error) = transfers.cancel(id).await
    {
        failures.push(format!("cancel: {}", safe_error(error)));
    }
    let queue_closed = transfers
        .shutdown_until(Instant::now() + Duration::from_secs(60))
        .await;
    let can_clean = queue_closed.is_ok() && exercise.is_ok();
    if let Err(error) = queue_closed {
        let retained_source = source.keep();
        eprintln!(
            "[incident-directory] source-retained={}",
            retained_source.display()
        );
        if let Err(record_error) = evidence.record(json!({"event":"source-retained",
            "path":retained_source}))
        {
            failures.push(format!("evidence write: {record_error}"));
        }
        failures.push(format!("queue shutdown: {}", safe_error(error)));
    }
    if created_directory && can_clean {
        if let Some(probe_lease) = &lease
            && let Err(error) = cleanup_directory_probe(probe_lease, &directory, &owner).await
        {
            failures.push(format!("owned remote cleanup: {error}"));
        }
    } else if created_directory {
        failures.push("private remote fixture retained for inspection".to_owned());
    }
    connection_cancel.cancel();
    if let Some(probe_lease) = lease
        && let Err(error) = probe_lease
            .close_until(
                Instant::now() + Duration::from_secs(10),
                "directory probe finished",
            )
            .await
    {
        failures.push(format!("probe lease close: {}", safe_error(error)));
    }
    if let Err(error) = connections
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await
    {
        failures.push(format!("SSH service shutdown: {}", safe_error(error)));
    }
    evidence.record(json!({"event":"cleanup-finished", "ok":failures.is_empty(),
        "failures":failures, "fresh_connect_attempts":loads.load(Ordering::Acquire)}))?;
    exercise?;
    if !failures.is_empty() {
        bail!("directory probe cleanup failed; see retained JSONL evidence");
    }
    Ok(())
}
