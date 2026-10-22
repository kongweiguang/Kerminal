//! 隔离 rootless Podman 的真实运维与容器文件端到端验收。
//!
//! 仅操作 `target/acceptance/linux/container-fixture.sh` 提供的私有 socket 和主容器；
//! 主容器、镜像、私有网络不由本测试创建或删除。测试只在容器 `/tmp` 下创建随机目录。
//! WSL 运行入口：
//! `cd /mnt/c/kong/personal-projects/Kerminal/代码/kerminal-gpui && bash target/acceptance/linux/container-fixture.sh run cargo +1.98.0 test --locked -p kerminal-infrastructure --test operations_podman_acceptance -- --ignored --nocapture`
//!
//! @author kongweiguang

use std::{
    error::Error,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyTrustReceipt, HostKeyTrustRequest,
        RemoteConnectionLease, RemoteConnectionService,
    },
    files::{FileAccessProvider, FileService, MAX_TEXT_FILE_READ_BYTES},
    operations::{
        CommandExecutionLimits, CommandExecutionTarget, CommandInvocation, ContainerRuntimeService,
        OperationsCommandExecutor,
    },
    sessions::{PlainTextSnapshotRequest, SessionService},
};
use kerminal_domain::{
    files::{
        ContainerRuntime, DeleteTargetKind, FileEndpoint, RemotePath, WriteTextRequest,
        WriteTextStrategy,
    },
    identity::HostId,
    operations::{ContainerListQuery, ContainerTarget},
    sessions::{SessionOwner, TerminalSize},
};
use kerminal_infrastructure::{
    files::JsonFileBookmarkRepository,
    operations::{
        ContainerFileProvider, ContainerSessionTransportFactory,
        StructuredOperationsCommandExecutor,
    },
    sessions::{SessionManager, SessionRuntimeConfig},
};
use tokio::{runtime::Handle, time::Instant};
use tokio_util::sync::CancellationToken;

const FIXTURE_NAME: &str = "kerminal-gpui-acceptance-20261016";
const FIXTURE_NETWORK: &str = "kerminal-gpui-acceptance-20261016";
const FIXTURE_IMAGE: &str = "docker.io/library/alpine:latest";
const FIXTURE_LABEL_KEY: &str = "kerminal.acceptance.fixture";
const FIXTURE_LABEL_VALUE: &str = "kerminal-container-fixture-20261016";
const PODMAN_CGROUP_V2_REQUIRED_MESSAGE: &str = "Podman 容器统计不可用：该环境需要 cgroup v2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PodmanCgroupVersion {
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PodmanStatsCapability {
    stats_supported: bool,
    cgroup_v2_required: bool,
}

struct PodmanFixture {
    container_id: String,
    image: String,
    image_id: String,
    commands: Arc<StructuredOperationsCommandExecutor>,
    sessions: Arc<SessionManager>,
    containers: ContainerRuntimeService,
    files: FileService,
    _private_root: tempfile::TempDir,
}

impl PodmanFixture {
    /// 严格校验 fixture wrapper 的隔离环境，再装配本机 CLI 目标和同一个会话 registry。
    ///
    /// Podman 的 immutable image ID 有时以 bare hex 输出；先统一表示法，才能保持镜像钉定比较而不依赖 CLI 格式差异。
    /// @author kongweiguang
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let engine = required_environment("KERM_ACCEPT_CONTAINER_ENGINE")?;
        let name = required_environment("KERM_ACCEPT_CONTAINER_NAME")?;
        let container_id = required_environment("KERM_ACCEPT_CONTAINER_ID")?;
        let image = required_environment("KERM_ACCEPT_CONTAINER_IMAGE")?;
        let image_id = required_environment("KERM_ACCEPT_CONTAINER_IMAGE_ID")?;
        let image_id = normalize_oci_image_id(&image_id)
            .ok_or_else(|| io_error("fixture image ID is not a valid SHA-256 digest"))?;
        let network = required_environment("KERM_ACCEPT_CONTAINER_NETWORK")?;
        let fixture_root =
            PathBuf::from(required_environment("KERM_ACCEPT_CONTAINER_FIXTURE_ROOT")?);
        let socket = required_environment("KERM_ACCEPT_CONTAINER_SOCKET")?;

        if engine != "podman"
            || name != FIXTURE_NAME
            || network != FIXTURE_NETWORK
            || image != FIXTURE_IMAGE
            || container_id.len() < 12
            || !container_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || std::env::var("CONTAINER_HOST").ok().as_deref() != Some(socket.as_str())
            || std::env::var("DOCKER_HOST").ok().as_deref() != Some(socket.as_str())
        {
            return Err(
                io_error("Podman acceptance requires the task-private fixture wrapper").into(),
            );
        }
        for variable in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            let value = PathBuf::from(required_environment(variable)?);
            if !value.starts_with(&fixture_root) {
                return Err(io_error("Podman fixture XDG paths are not isolated").into());
            }
        }

        let remote: Arc<dyn RemoteConnectionService> = Arc::new(NoRemoteConnections);
        let commands = Arc::new(StructuredOperationsCommandExecutor::new(remote.clone()));
        let command_port: Arc<dyn OperationsCommandExecutor> = commands.clone();
        let session_factory = Arc::new(ContainerSessionTransportFactory::new(remote));
        let sessions = Arc::new(SessionManager::new(
            session_factory,
            Handle::current(),
            SessionRuntimeConfig::default(),
        ));
        let session_port: Arc<dyn SessionService> = sessions.clone();
        let containers = ContainerRuntimeService::new(command_port.clone(), session_port);
        let private_root = tempfile::tempdir()?;
        let provider: Arc<dyn FileAccessProvider> =
            Arc::new(ContainerFileProvider::new(command_port));
        let bookmarks = Arc::new(JsonFileBookmarkRepository::new(private_root.path()));
        let files = FileService::new(vec![provider], bookmarks)?;

        Ok(Self {
            container_id,
            image,
            image_id,
            commands,
            sessions,
            containers,
            files,
            _private_root: private_root,
        })
    }

    /// 构造本机 Podman endpoint；容器 ID 始终来自隔离 fixture 的只读元数据。
    fn target(&self) -> ContainerTarget {
        ContainerTarget {
            host_id: None,
            runtime: ContainerRuntime::Podman,
            container_id: self.container_id.clone(),
        }
    }

    /// 构造容器文件 endpoint 时统一经过容器路径规范化规则。
    fn file_endpoint(&self, path: &str) -> Result<FileEndpoint, Box<dyn Error>> {
        Ok(FileEndpoint::Container {
            host_id: None,
            runtime: ContainerRuntime::Podman,
            container_id: self.container_id.clone(),
            path: RemotePath::parse_container(path)
                .map_err(|_| io_error("container acceptance path is invalid"))?,
        })
    }
}

/// 此 fixture 只访问本机 Podman；误触 SSH 路径立即报错，不创建连接或改 known_hosts。
struct NoRemoteConnections;

#[async_trait]
impl RemoteConnectionService for NoRemoteConnections {
    /// 本验收不允许容器 CLI 探测连接 SSH 主机。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("Podman acceptance disables SSH"))
    }

    /// 本验收不读取外部 known_hosts。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("Podman acceptance disables SSH"))
    }

    /// 本验收不信任或写入任何 SSH 主机密钥。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("Podman acceptance disables SSH"))
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

/// 验证真实 Podman 的统计能力，并在已识别的 cgroup v1 限制下继续完整文件与会话流程。
///
/// cgroup 版本从隔离 socket 的 `podman info` 读取；仅匹配精确的安全错误文案才允许 v1 继续。
/// 清理失败只报告固定阶段名，避免把主机命令诊断内容写入 acceptance 日志。
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private rootless Podman fixture; see this file's environment contract"]
async fn operations_container_and_file_protocol_round_trip() -> Result<(), Box<dyn Error>> {
    let fixture = PodmanFixture::from_environment()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let scratch = format!(
        "/tmp/kerminal-gpui-acceptance-{}-{nonce}",
        std::process::id()
    );
    let scratch_endpoint = fixture.file_endpoint(&scratch)?;
    let text_endpoint = fixture.file_endpoint(&format!("{scratch}/sub/config 'quoted'.txt"))?;

    let exercise = exercise_podman_operations(&fixture, &text_endpoint).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let sessions_cleanup = fixture
        .sessions
        .close_all_until(deadline)
        .await
        .map_err(|_| io_error("Podman acceptance stage: session cleanup"));
    let files_cleanup = fixture
        .files
        .delete(
            CancellationToken::new(),
            scratch_endpoint.clone(),
            DeleteTargetKind::Directory,
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: scratch cleanup"));
    let scratch_readback = fixture
        .files
        .list_directory(CancellationToken::new(), scratch_endpoint)
        .await;
    let container_readback = fixture
        .containers
        .inspect(fixture.target(), CancellationToken::new())
        .await
        .map_err(|_| io_error("Podman acceptance stage: final container inspect"));
    fixture.commands.begin_shutdown(deadline);
    let commands_cleanup = fixture
        .commands
        .shutdown_until(deadline)
        .await
        .map_err(|_| io_error("Podman acceptance stage: command executor cleanup"));

    let stats_capability = exercise?;
    println!(
        "Podman acceptance: statsSupported={}, cgroupV2Required={}",
        stats_capability.stats_supported, stats_capability.cgroup_v2_required
    );
    sessions_cleanup?;
    files_cleanup?;
    match scratch_readback {
        Err(error) if error.public_code() == kerminal_application::PublicErrorCode::NotFound => {}
        Ok(_) => return Err(io_error("Podman scratch directory remained after cleanup").into()),
        Err(error) => return Err(error.into()),
    }
    let container = container_readback?;
    if !container.running || !container.id.eq_ignore_ascii_case(&fixture.container_id) {
        return Err(io_error("fixture container changed during operations acceptance").into());
    }
    commands_cleanup?;
    Ok(())
}

/// 调用实际 container/file/session services；统计只在 cgroup v1 的精确受限错误下可跳过。
///
/// 其它 stats 失败必须使本验收失败，同时允许真实 logs、交互 exec 与文件操作在已知限制下继续。
/// 分类失败时只输出本地枚举、稳定错误码与固定文案是否匹配，避免把远端 stderr 带入日志。
/// @author kongweiguang
async fn exercise_podman_operations(
    fixture: &PodmanFixture,
    text_endpoint: &FileEndpoint,
) -> Result<PodmanStatsCapability, Box<dyn Error>> {
    verify_fixture_image(fixture)
        .await
        .map_err(|_| io_error("Podman acceptance stage: fixture image verification"))?;
    let target = fixture.target();
    let listed = fixture
        .containers
        .list(
            ContainerListQuery {
                host_id: None,
                runtime: ContainerRuntime::Podman,
                include_stopped: true,
            },
            CancellationToken::new(),
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: container list"))?;
    if !listed.iter().any(|entry| {
        entry
            .target
            .container_id
            .eq_ignore_ascii_case(&fixture.container_id)
            && entry.name.trim_start_matches('/') == FIXTURE_NAME
            && entry
                .labels
                .iter()
                .any(|(key, value)| key == FIXTURE_LABEL_KEY && value == FIXTURE_LABEL_VALUE)
    }) {
        return Err(io_error("isolated fixture container was absent from Podman list").into());
    }

    let inspect = fixture
        .containers
        .inspect(target.clone(), CancellationToken::new())
        .await
        .map_err(|_| io_error("Podman acceptance stage: container inspect"))?;
    if !inspect.running
        || !inspect.id.eq_ignore_ascii_case(&fixture.container_id)
        || inspect.image != fixture.image
    {
        return Err(io_error("Podman inspect did not identify the running fixture").into());
    }
    let cgroup_version = read_podman_cgroup_version(fixture)
        .await
        .map_err(|_| io_error("Podman acceptance stage: cgroup metadata"))?;
    let stats_capability = match fixture
        .containers
        .stats(target.clone(), CancellationToken::new())
        .await
    {
        Ok(stats) => {
            if stats.raw.trim().is_empty() {
                return Err(io_error("Podman stats returned no runtime snapshot").into());
            }
            PodmanStatsCapability {
                stats_supported: true,
                cgroup_v2_required: false,
            }
        }
        Err(error)
            if cgroup_version == PodmanCgroupVersion::V1
                && error.public_code() == PublicErrorCode::RemoteExit
                && error.public_message() == PODMAN_CGROUP_V2_REQUIRED_MESSAGE =>
        {
            PodmanStatsCapability {
                stats_supported: false,
                cgroup_v2_required: true,
            }
        }
        Err(error) => {
            let cgroup_version = match cgroup_version {
                PodmanCgroupVersion::V1 => "v1",
                PodmanCgroupVersion::V2 => "v2",
            };
            let expected_message = error.public_message() == PODMAN_CGROUP_V2_REQUIRED_MESSAGE;
            return Err(io_error(&format!(
                "Podman acceptance stage: container stats (cgroupVersion={cgroup_version}, publicCode={}, expectedCgroupV2Message={expected_message})",
                error.public_code().as_str(),
            ))
            .into());
        }
    };
    let logs = fixture
        .containers
        .logs(target.clone(), 20, CancellationToken::new())
        .await
        .map_err(|_| io_error("Podman acceptance stage: container logs"))?;
    if logs.target != target || logs.truncated {
        return Err(io_error("Podman tail logs returned a mismatched or truncated result").into());
    }

    let marker = format!("KERMINAL_PODMAN_EXEC_{}", std::process::id());
    let session = fixture
        .containers
        .open_exec(
            target,
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("printf '%s\\n' '{marker}'; sleep 20"),
            ],
            SessionOwner::Service {
                name: "operations-podman-acceptance".to_owned(),
            },
            TerminalSize::default(),
            CancellationToken::new(),
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: container exec session open"))?;
    wait_for_session_marker(fixture.sessions.as_ref(), &session.session_id, &marker)
        .await
        .map_err(|_| io_error("Podman acceptance stage: container exec output"))?;
    fixture
        .sessions
        .close_until(session.session_id, Instant::now() + Duration::from_secs(12))
        .await
        .map_err(|_| io_error("Podman acceptance stage: container exec session close"))?;

    let directory = match text_endpoint {
        FileEndpoint::Container { path, .. } => path.parent(),
        _ => None,
    }
    .ok_or_else(|| io_error("scratch file endpoint has no directory"))?;
    let directory_endpoint = FileEndpoint::Container {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: fixture.container_id.clone(),
        path: directory.clone(),
    };
    fixture
        .files
        .create_directory(CancellationToken::new(), directory_endpoint.clone())
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file create directory"))?;

    let original = "container acceptance: first revision\n";
    fixture
        .files
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: text_endpoint.clone(),
                content: original.to_owned(),
                strategy: WriteTextStrategy::CreateNew,
            },
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file create"))?;
    let opened = fixture
        .files
        .read_text_file(
            CancellationToken::new(),
            text_endpoint.clone(),
            MAX_TEXT_FILE_READ_BYTES,
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file read"))?;
    if opened.content != original || opened.truncated {
        return Err(io_error("container text read did not return the created content").into());
    }

    let updated = "container acceptance: compare-and-swap revision\n";
    fixture
        .files
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: text_endpoint.clone(),
                content: updated.to_owned(),
                strategy: WriteTextStrategy::CompareRevision(opened.revision),
            },
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file compare-and-swap write"))?;
    let preview = fixture
        .files
        .preview_file(CancellationToken::new(), text_endpoint.clone(), 256)
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file preview"))?;
    if preview.binary || preview.truncated || preview.bytes != updated.as_bytes() {
        return Err(io_error("container preview did not preserve the updated text bytes").into());
    }

    let listing = fixture
        .files
        .list_directory(CancellationToken::new(), directory_endpoint.clone())
        .await
        .map_err(|_| io_error("Podman acceptance stage: container directory list"))?;
    let expected_name = "config 'quoted'.txt";
    let entry = listing
        .entries
        .iter()
        .find(|entry| entry.name == expected_name)
        .ok_or_else(|| io_error("BusyBox-compatible directory listing missed a quoted filename"))?;
    if entry.raw.is_some() {
        return Err(io_error("container listing fabricated protocol raw metadata").into());
    }

    fixture
        .files
        .chmod(CancellationToken::new(), text_endpoint.clone(), 0o600)
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file chmod"))?;
    let renamed = fixture.file_endpoint(&format!("{directory}/renamed config.txt"))?;
    fixture
        .files
        .rename(
            CancellationToken::new(),
            text_endpoint.clone(),
            renamed.clone(),
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file rename"))?;
    let renamed_document = fixture
        .files
        .read_text_file(
            CancellationToken::new(),
            renamed.clone(),
            MAX_TEXT_FILE_READ_BYTES,
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: renamed container file read"))?;
    if renamed_document.content != updated {
        return Err(io_error("container rename did not preserve file content").into());
    }
    fixture
        .files
        .delete(CancellationToken::new(), renamed, DeleteTargetKind::File)
        .await
        .map_err(|_| io_error("Podman acceptance stage: container file delete"))?;
    fixture
        .files
        .delete(
            CancellationToken::new(),
            directory_endpoint,
            DeleteTargetKind::Directory,
        )
        .await
        .map_err(|_| io_error("Podman acceptance stage: container directory delete"))?;
    Ok(stats_capability)
}

/// 从 fixture wrapper 指向的 Podman socket 读取 cgroup 版本；未知格式必须阻断而非猜测。
///
/// 只解析 `host.cgroupVersion` 这一个字段，避免把其它主机信息写进验收输出。
/// @author kongweiguang
async fn read_podman_cgroup_version(
    fixture: &PodmanFixture,
) -> Result<PodmanCgroupVersion, Box<dyn Error>> {
    let output = fixture
        .commands
        .execute(
            CommandExecutionTarget::Local,
            CommandInvocation::new(
                "podman",
                vec!["info".to_owned(), "--format".to_owned(), "json".to_owned()],
            ),
            CommandExecutionLimits {
                timeout: Duration::from_secs(10),
                max_output_bytes: 1024 * 1024,
            },
            CancellationToken::new(),
        )
        .await?;
    if output.exit_code != Some(0) || output.truncated {
        return Err(io_error("Podman info did not return complete cgroup metadata").into());
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| io_error("Podman info JSON could not be parsed"))?;
    match document
        .get("host")
        .and_then(|host| host.get("cgroupVersion"))
        .and_then(serde_json::Value::as_str)
    {
        Some("v1") => Ok(PodmanCgroupVersion::V1),
        Some("v2") => Ok(PodmanCgroupVersion::V2),
        _ => Err(io_error("Podman info has an unknown cgroup version").into()),
    }
}

/// 通过真实运行时按固定 tag 回读不可变 image ID，归一化格式后仍严格匹配 fixture 钉定值。
/// @author kongweiguang
async fn verify_fixture_image(fixture: &PodmanFixture) -> Result<(), Box<dyn Error>> {
    let result = fixture
        .commands
        .execute(
            CommandExecutionTarget::Local,
            CommandInvocation::new(
                "podman",
                vec![
                    "image".to_owned(),
                    "inspect".to_owned(),
                    "--format".to_owned(),
                    "{{.Id}}".to_owned(),
                    fixture.image.clone(),
                ],
            ),
            CommandExecutionLimits {
                timeout: Duration::from_secs(10),
                max_output_bytes: 4096,
            },
            CancellationToken::new(),
        )
        .await?;
    let image_id = std::str::from_utf8(&result.stdout)?.trim();
    let image_id = normalize_oci_image_id(image_id);
    if result.exit_code != Some(0)
        || image_id.as_deref() != Some(fixture.image_id.as_str())
        || result.truncated
    {
        return Err(
            io_error("fixture image reference did not resolve to its pinned image ID").into(),
        );
    }
    Ok(())
}

/// 有界轮询 terminal engine 快照，确认 marker 来自实际 Podman exec PTY 输出。
async fn wait_for_session_marker(
    sessions: &dyn SessionService,
    session_id: &kerminal_domain::identity::SessionId,
    marker: &str,
) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let snapshot = sessions
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 128,
                        max_bytes: Some(16 * 1024),
                    },
                )
                .await?;
            if snapshot.text.contains(marker) {
                return Ok::<(), kerminal_application::AppError>(());
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .map_err(|_| io_error("Podman exec terminal marker timed out"))??;
    Ok(())
}

/// 从 fixture wrapper 读取非空变量；错误消息只说明字段名，不回显 socket 或环境数据。
fn required_environment(name: &str) -> Result<String, Box<dyn Error>> {
    let value =
        std::env::var(name).map_err(|_| io_error(&format!("missing fixture field {name}")))?;
    if value.trim().is_empty() {
        return Err(io_error("Podman fixture metadata contains an empty value").into());
    }
    Ok(value)
}

/// 将验收失败转换为不带凭据的简洁错误，方便 H/P 在远程日志中定位。
fn io_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_owned())
}

/// 将 Podman 的 bare 或带前缀 SHA-256 ID 变成唯一小写形式，仍拒绝不完整或其它算法的身份值。
/// @author kongweiguang
fn normalize_oci_image_id(value: &str) -> Option<String> {
    let digest = match value.strip_prefix("sha256:") {
        Some(digest) => digest,
        None if value.len() == 64 => value,
        None => return None,
    };
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("sha256:{}", digest.to_ascii_lowercase()))
}

/// 固定不同 CLI 输出形式的等价关系，同时确保 malformed digest 仍在 fixture 边界被拒绝。
/// @author kongweiguang
#[test]
fn normalizes_only_valid_sha256_image_ids() {
    let digest = "0123456789abcdef".repeat(4);
    let uppercase_digest = digest.to_ascii_uppercase();
    let expected = format!("sha256:{digest}");
    let cases = [
        ("bare lowercase", digest.clone(), Some(expected.clone())),
        (
            "prefixed lowercase",
            format!("sha256:{digest}"),
            Some(expected.clone()),
        ),
        (
            "bare uppercase",
            uppercase_digest.clone(),
            Some(expected.clone()),
        ),
        (
            "prefixed uppercase digest",
            format!("sha256:{uppercase_digest}"),
            Some(expected),
        ),
        ("short digest", digest[..63].to_owned(), None),
        ("long digest", format!("{digest}0"), None),
        ("non-hex digest", format!("{}g", &digest[..63]), None),
        ("wrong algorithm", format!("sha512:{digest}"), None),
    ];

    for (case, input, expected) in cases {
        assert_eq!(normalize_oci_image_id(&input), expected, "case: {case}");
    }
}
