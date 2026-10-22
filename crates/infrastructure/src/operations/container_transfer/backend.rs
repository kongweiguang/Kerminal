//! 容器传输后端的路由、目标选择和错误分类。
//!
//! @author kongweiguang

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        ContainerStageCleanupCapability, OperationsCommandExecutor, OperationsCommandStream,
    },
    transfers::{
        TransferBackend, TransferCleanupState, TransferExecutionContext, TransferExecutionError,
        TransferExecutionFailure, TransferExecutionResult, TransferPhase, TransferProgressUpdate,
        TransferResourceCleanup,
    },
};
use kerminal_domain::files::{ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind};
use kerminal_domain::identity::TransferId;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{download::download, model::*, upload::upload};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_OUTPUT_LIMIT: usize = 64 * 1024;
pub(super) const STREAM_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) const CREATE_STAGE_SCRIPT: &str = "set -eu; parent=$1; stage_name=$2; owner=$3; stage=\"${parent%/}/$stage_name\"; [ -d \"$parent\" ] && [ ! -L \"$parent\" ] || exit 40; [ ! -e \"$stage\" ] && [ ! -L \"$stage\" ] || exit 41; mkdir -m 700 -- \"$stage\" || exit 42; marker=\"$stage/.kerminal-owner\"; (set -C; printf '%s\\n' \"$owner\" > \"$marker\") || exit 43; [ -f \"$marker\" ] && [ ! -L \"$marker\" ] && [ \"$(cat -- \"$marker\")\" = \"$owner\" ] || exit 44";
pub(crate) const CLEANUP_STAGE_SCRIPT: &str = "set -eu; parent=$1; stage_name=$2; owner=$3; stage=\"${parent%/}/$stage_name\"; marker=\"$stage/.kerminal-owner\"; if [ ! -e \"$stage\" ] && [ ! -L \"$stage\" ]; then exit 0; fi; [ -d \"$stage\" ] && [ ! -L \"$stage\" ] || exit 44; [ -f \"$marker\" ] && [ ! -L \"$marker\" ] || exit 45; [ \"$(cat -- \"$marker\")\" = \"$owner\" ] || exit 46; rm -rf -- \"$stage\" || exit 47; [ ! -e \"$stage\" ] && [ ! -L \"$stage\" ] || exit 48";

/// 由唯一运维命令端口执行容器文件传输，凭据、SSH route 与任务队列都由既有服务管理。
pub struct ContainerTransferBackend {
    pub(super) commands: Arc<dyn OperationsCommandExecutor>,
}

impl ContainerTransferBackend {
    /// 装配时与 SFTP backend 一同注入 TransferService，不创建容器专用队列。
    pub fn new(commands: Arc<dyn OperationsCommandExecutor>) -> Self {
        Self { commands }
    }
}

#[async_trait]
impl TransferBackend for ContainerTransferBackend {
    /// 仅接本机与容器之间的单段传输；远程 SSH/SFTP 仍交给统一 SFTP backend。
    fn supports(&self, request: &kerminal_domain::files::TransferRequest) -> bool {
        matches!(
            (&request.source, &request.destination),
            (FileEndpoint::Local { .. }, FileEndpoint::Container { .. })
                | (FileEndpoint::Container { .. }, FileEndpoint::Local { .. })
        )
    }

    /// 对跨队列重试固定实际目标；非 seekable tar stream 不伪造可信断点。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        if context.cancellation.is_cancelled() {
            return Err(TransferExecutionError::Cancelled {
                checkpoint: None,
                actual_destination: context.actual_destination,
            });
        }
        if context.resume_from.is_some() {
            return Err(failed(
                TransferFailureKind::Unsupported,
                false,
                context.actual_destination,
            ));
        }
        if !self.supports(&context.request) {
            return Err(failed(
                TransferFailureKind::Unsupported,
                false,
                context.actual_destination,
            ));
        }
        match (&context.request.source, &context.request.destination) {
            (FileEndpoint::Local { path }, FileEndpoint::Container { .. }) => {
                upload(self.commands.as_ref(), &context, path)
                    .await
                    .map_err(|error| *error)
            }
            (FileEndpoint::Container { .. }, FileEndpoint::Local { path }) => {
                download(self.commands.as_ref(), &context, path)
                    .await
                    .map_err(|error| *error)
            }
            _ => Err(failed(
                TransferFailureKind::Unsupported,
                false,
                context.actual_destination,
            )),
        }
    }
}

/// 统一创建无断点后端失败，容器 tar 管道不声明无法验证的续传能力。
fn failed(
    kind: TransferFailureKind,
    retryable: bool,
    destination: Option<FileEndpoint>,
) -> TransferExecutionError {
    TransferExecutionError::Failed(TransferExecutionFailure::new(
        kind,
        retryable,
        false,
        None,
        destination,
        TransferCleanupState::Released,
    ))
}

/// 构造容器内 sh -c 固定脚本调用；路径、标识和策略始终占独立 argv 槽位。
pub(super) fn container_invocation(
    endpoint: &FileEndpoint,
    script: &'static str,
    arguments: Vec<String>,
) -> AppResult<(CommandExecutionTarget, CommandInvocation)> {
    let target = container_endpoint(endpoint)?;
    let mut command_arguments = vec![
        "exec".to_owned(),
        "--interactive".to_owned(),
        target.container_id.to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        script.to_owned(),
        "kerminal".to_owned(),
    ];
    command_arguments.extend(arguments);
    Ok((
        command_target(target.host_id),
        CommandInvocation::new(runtime_program(target.runtime), command_arguments),
    ))
}

/// 把 transfer ID 映射为固定长度 owner marker，避免不透明 ID 进入路径或脚本语法。
pub(crate) fn stage_owner_marker(transfer_id: &TransferId) -> String {
    let digest = Sha256::digest(transfer_id.as_str().as_bytes());
    let compact = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("kerminal-transfer-{compact}")
}

/// 用同一容器 endpoint 构造专属 staging 命令，所有动态值仍是独立 argv 参数。
pub(crate) fn stage_cleanup_invocation(
    endpoint: &FileEndpoint,
    owner: &str,
    script: &'static str,
) -> AppResult<(CommandExecutionTarget, CommandInvocation)> {
    let (parent, stage_name) = remote_parent_name(endpoint)?;
    container_invocation(
        endpoint,
        script,
        vec![parent.as_str().to_owned(), stage_name, owner.to_owned()],
    )
}

/// 用固定小预算读取 metadata 或提交回执；输出被截断时不解析为成功。
pub(super) async fn execute_control(
    commands: &dyn OperationsCommandExecutor,
    endpoint: &FileEndpoint,
    script: &'static str,
    arguments: Vec<String>,
    cancellation: CancellationToken,
) -> AppResult<CommandExecutionResult> {
    let (target, invocation) = container_invocation(endpoint, script, arguments)?;
    let result = commands
        .execute(
            target,
            invocation,
            CommandExecutionLimits {
                timeout: CONTROL_TIMEOUT,
                max_output_bytes: CONTROL_OUTPUT_LIMIT,
            },
            cancellation,
        )
        .await?;
    if result.truncated {
        return Err(AppError::infrastructure("容器传输控制命令输出超过安全上限"));
    }
    Ok(result)
}

/// 为文件流启动有界双向 worker，长传输依靠队列 idle deadline 取消而非缓冲整个文件。
pub(super) async fn open_stream(
    commands: &dyn OperationsCommandExecutor,
    endpoint: &FileEndpoint,
    script: &'static str,
    arguments: Vec<String>,
    cancellation: CancellationToken,
) -> AppResult<kerminal_application::operations::OperationsCommandStream> {
    let (target, invocation) = container_invocation(endpoint, script, arguments)?;
    commands
        .execute_stream(
            target,
            invocation,
            CommandExecutionLimits {
                timeout: STREAM_TIMEOUT,
                max_output_bytes: CONTROL_OUTPUT_LIMIT,
            },
            cancellation,
        )
        .await
}

/// 根据明确退出码分类容器端脚本失败，只有固定前置检查失败可断言未发生提交。
pub(super) fn ensure_success(result: &CommandExecutionResult) -> AppResult<()> {
    if result.truncated {
        return Err(AppError::infrastructure("容器传输控制输出不完整"));
    }
    match result.exit_code {
        Some(0) => Ok(()),
        Some(40) => Err(AppError::not_found("容器传输源或目标不存在")),
        Some(41) => Err(AppError::conflict("容器传输目标发生冲突")),
        Some(42) => Err(AppError::conflict("容器传输路径类型无效")),
        Some(127) => Err(AppError::unsupported(
            "容器需要 POSIX sh、tar 与 GNU fileutils",
        )),
        code => Err(AppError::remote_exit(code)),
    }
}

/// 读取容器路径基础类型；不跟随链接，文件大小可作为准确传输总量。
pub(super) async fn probe_container(
    commands: &dyn OperationsCommandExecutor,
    endpoint: &FileEndpoint,
    path: &str,
    cancellation: CancellationToken,
) -> AppResult<RemoteObject> {
    const SCRIPT: &str = "set -eu; p=$1; if [ -L \"$p\" ]; then printf 'link\\n'; elif [ -f \"$p\" ]; then printf 'file\\n'; stat -c '%s' -- \"$p\"; elif [ -d \"$p\" ]; then printf 'directory\\n'; elif [ -e \"$p\" ]; then printf 'other\\n'; else printf 'missing\\n'; fi";
    let result = execute_control(
        commands,
        endpoint,
        SCRIPT,
        vec![path.to_owned()],
        cancellation,
    )
    .await?;
    ensure_success(&result)?;
    let mut lines = result.stdout.split(|byte| *byte == b'\n');
    let kind = match lines.next().unwrap_or_default() {
        b"file" => RemoteObjectKind::File,
        b"directory" => RemoteObjectKind::Directory,
        b"link" => RemoteObjectKind::Symlink,
        b"missing" => RemoteObjectKind::Missing,
        _ => RemoteObjectKind::Other,
    };
    let size = if kind == RemoteObjectKind::File {
        Some(
            std::str::from_utf8(lines.next().unwrap_or_default())
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .ok_or_else(|| AppError::infrastructure("容器文件长度回执无效"))?,
        )
    } else {
        None
    };
    Ok(RemoteObject { kind, size })
}

/// 目标探测只比较普通文件/目录；链接和特殊对象不能被覆盖或用于逃逸路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteObjectKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

/// 容器目标基础元数据不缓存权限或凭据，仅供传输冲突决策使用。
#[derive(Debug, Clone, Copy)]
pub(super) struct RemoteObject {
    pub kind: RemoteObjectKind,
    pub size: Option<u64>,
}

/// 选择一次并固定冲突策略结果；Skip 不打开流，Rename 只在原目录寻找一个空目标。
pub(super) async fn select_remote_destination(
    commands: &dyn OperationsCommandExecutor,
    requested: &FileEndpoint,
    requested_kind: TransferKind,
    policy: ConflictPolicy,
    cancellation: CancellationToken,
    pinned: Option<&FileEndpoint>,
) -> AppResult<Option<FileEndpoint>> {
    let endpoint = pinned.unwrap_or(requested);
    let target = container_endpoint(endpoint)?;
    let existing = probe_container(
        commands,
        endpoint,
        target.path.as_str(),
        cancellation.clone(),
    )
    .await?;
    if pinned.is_some() {
        if existing.kind != RemoteObjectKind::Missing && policy != ConflictPolicy::Overwrite {
            return Err(AppError::conflict("固定的传输目标在重试前已出现冲突"));
        }
        validate_existing_kind(existing, requested_kind, policy)?;
        return Ok(Some(endpoint.clone()));
    }
    if existing.kind == RemoteObjectKind::Symlink || existing.kind == RemoteObjectKind::Other {
        return Err(AppError::conflict("容器目标不是可安全复制的普通对象"));
    }
    if existing.kind == RemoteObjectKind::Missing {
        return Ok(Some(endpoint.clone()));
    }
    match policy {
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Overwrite => {
            validate_existing_kind(existing, requested_kind, policy)?;
            Ok(Some(endpoint.clone()))
        }
        ConflictPolicy::Rename => {
            for index in 1..=10_000_u32 {
                let candidate_path = renamed_remote_candidate(target.path, index)?;
                let candidate = FileEndpoint::Container {
                    host_id: target.host_id.cloned(),
                    runtime: target.runtime,
                    container_id: target.container_id.to_owned(),
                    path: candidate_path,
                };
                let candidate_target = container_endpoint(&candidate)?;
                let candidate_probe = probe_container(
                    commands,
                    &candidate,
                    candidate_target.path.as_str(),
                    cancellation.clone(),
                )
                .await?;
                if candidate_probe.kind == RemoteObjectKind::Missing {
                    return Ok(Some(candidate));
                }
            }
            Err(AppError::conflict("无法为容器传输选择稳定的新名称"))
        }
    }
}

/// 为本机目标规范化父目录并完成与远端相同的顶层冲突选择。
///
/// @author kongweiguang
pub(super) fn select_local_destination(
    requested: &Path,
    requested_kind: TransferKind,
    policy: ConflictPolicy,
    pinned: Option<&FileEndpoint>,
) -> AppResult<Option<PathBuf>> {
    let normalized = canonical_local_target(match pinned {
        Some(FileEndpoint::Local { path }) => path,
        Some(_) => return Err(AppError::invalid("重试目标端点类型发生变化")),
        None => requested,
    })?;
    let existing = fs::symlink_metadata(&normalized);
    let exists = match existing {
        Ok(metadata) => Some(if metadata.file_type().is_symlink() {
            RemoteObjectKind::Symlink
        } else if metadata.is_file() {
            RemoteObjectKind::File
        } else if metadata.is_dir() {
            RemoteObjectKind::Directory
        } else {
            RemoteObjectKind::Other
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_failure("检查本机传输目标失败", error)),
    };
    if pinned.is_some() {
        let object = exists.unwrap_or(RemoteObjectKind::Missing);
        if object != RemoteObjectKind::Missing && policy != ConflictPolicy::Overwrite {
            return Err(AppError::conflict("固定的本机传输目标在重试前已出现冲突"));
        }
        validate_existing_kind(
            RemoteObject {
                kind: object,
                size: None,
            },
            requested_kind,
            policy,
        )?;
        return Ok(Some(normalized));
    }
    let Some(kind) = exists else {
        return Ok(Some(normalized));
    };
    if kind == RemoteObjectKind::Symlink || kind == RemoteObjectKind::Other {
        return Err(AppError::conflict("本机目标不是可安全复制的普通对象"));
    }
    match policy {
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Overwrite => {
            validate_existing_kind(RemoteObject { kind, size: None }, requested_kind, policy)?;
            Ok(Some(normalized))
        }
        ConflictPolicy::Rename => {
            for index in 1..=10_000_u32 {
                let candidate = renamed_local_candidate(&normalized, index)?;
                match fs::symlink_metadata(&candidate) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok(Some(candidate));
                    }
                    Ok(_) => continue,
                    Err(error) => return Err(io_failure("检查本机重命名目标失败", error)),
                }
            }
            Err(AppError::conflict("无法为本机传输选择稳定的新名称"))
        }
    }
}

/// 顶层 Skip/Overwrite 和目录 merge 都不允许文件与目录互相代替，也不跟随符号链接。
fn validate_existing_kind(
    existing: RemoteObject,
    requested: TransferKind,
    policy: ConflictPolicy,
) -> AppResult<()> {
    if existing.kind == RemoteObjectKind::Missing {
        return Ok(());
    }
    if existing.kind == RemoteObjectKind::Other || existing.kind == RemoteObjectKind::Symlink {
        return Err(AppError::conflict("传输目标类型不受支持"));
    }
    if policy == ConflictPolicy::Rename {
        return Ok(());
    }
    let matches = matches!(
        (requested, existing.kind),
        (TransferKind::File, RemoteObjectKind::File)
            | (TransferKind::Directory, RemoteObjectKind::Directory)
    );
    if matches {
        Ok(())
    } else {
        Err(AppError::conflict("传输目标类型与源类型不匹配"))
    }
}

/// 可靠进度仅包含实际复制的数据字节；已提交 entry/bytes 由 queue receipt 独立统计。
pub(super) fn report_progress(
    context: &TransferExecutionContext,
    bytes: u64,
    total_bytes: Option<u64>,
    destination: &FileEndpoint,
    phase: TransferPhase,
) {
    context.progress.report(TransferProgressUpdate {
        confirmed_bytes: bytes,
        total_bytes,
        phase,
        actual_destination: Some(destination.clone()),
        checkpoint: None,
        ..TransferProgressUpdate::default()
    });
}

/// 建立安全失败投影；部分提交后的连接问题不再允许整项自动重放。
pub(super) fn app_failure(
    error: AppError,
    destination: Option<FileEndpoint>,
    retryable: bool,
) -> TransferExecutionError {
    app_failure_with_cleanup(
        error,
        destination,
        retryable,
        TransferCleanupState::Released,
    )
}

/// 取消只有在资源释放得到确认后才映射为 Cancelled，否则保留 Pending owner 的失败态。
pub(super) fn app_failure_with_cleanup(
    error: AppError,
    destination: Option<FileEndpoint>,
    retryable: bool,
    cleanup: TransferCleanupState,
) -> TransferExecutionError {
    if error.public_code() == PublicErrorCode::Cancelled && cleanup.is_released() {
        return TransferExecutionError::Cancelled {
            checkpoint: None,
            actual_destination: destination,
        };
    }
    let kind = match error.public_code() {
        PublicErrorCode::Invalid | PublicErrorCode::NotFound => TransferFailureKind::InvalidSource,
        PublicErrorCode::Conflict => TransferFailureKind::Conflict,
        PublicErrorCode::Unsupported | PublicErrorCode::CapabilityNotSupported => {
            TransferFailureKind::Unsupported
        }
        PublicErrorCode::AuthenticationFailed
        | PublicErrorCode::HostKeyUnknown
        | PublicErrorCode::HostKeyChanged
        | PublicErrorCode::HostKeyTrustMismatch
        | PublicErrorCode::Infrastructure => TransferFailureKind::Connection,
        PublicErrorCode::Timeout => TransferFailureKind::IdleTimeout,
        PublicErrorCode::OutcomeUnknown => TransferFailureKind::CommitUnknown,
        _ => TransferFailureKind::Internal,
    };
    let retryable = retryable
        && cleanup.is_released()
        && matches!(
            kind,
            TransferFailureKind::Connection | TransferFailureKind::IdleTimeout
        );
    TransferExecutionError::Failed(TransferExecutionFailure::new(
        kind,
        retryable,
        false,
        None,
        destination,
        cleanup,
    ))
}

/// 单独保留 staging 能力，以便 TransferService 在普通 command intake 关闭后重试清理。
pub(super) struct ContainerStageCleanupOwner {
    capability: Arc<dyn ContainerStageCleanupCapability>,
}

impl ContainerStageCleanupOwner {
    /// 只保存预签且绑定 endpoint 的能力，不保存可供调用者改写的路径或命令参数。
    pub(super) fn new(capability: Arc<dyn ContainerStageCleanupCapability>) -> Self {
        Self { capability }
    }
}

#[async_trait]
impl ContainerPostStreamCleanup for ContainerStageCleanupOwner {
    /// stream worker 确认关闭后才能删除该 worker 专属的远端 staging。
    async fn cleanup_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.capability.cleanup_until(deadline).await
    }
}

#[async_trait]
impl TransferResourceCleanup for ContainerStageCleanupOwner {
    /// 供 TransferService 保存资源 owner，并在同一能力上以新的 absolute deadline 重试。
    async fn cleanup_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.capability.cleanup_until(deadline).await
    }
}

/// 先直接尝试清理 staging；失败时返回保有同一 capability 的队列 owner。
pub(super) async fn cleanup_stage_capability(
    capability: Arc<dyn ContainerStageCleanupCapability>,
    deadline: tokio::time::Instant,
) -> (AppResult<()>, TransferCleanupState) {
    match capability.cleanup_until(deadline).await {
        Ok(()) => (Ok(()), TransferCleanupState::Released),
        Err(error) => (
            Err(error),
            TransferCleanupState::pending(Arc::new(ContainerStageCleanupOwner::new(capability))),
        ),
    }
}

/// 传输 worker 仍持有资源时，让队列保存可重试清理的真实 OperationsCommandStream owner。
pub(super) struct ContainerStreamCleanupOwner {
    stream: Arc<Mutex<OperationsCommandStream>>,
    after_close: Option<Arc<dyn ContainerPostStreamCleanup>>,
}

impl ContainerStreamCleanupOwner {
    /// 克隆同一个 stream Arc，确保失败结果和后续清理重试共用真实 worker。
    pub(super) fn new(stream: Arc<Mutex<OperationsCommandStream>>) -> Self {
        Self {
            stream,
            after_close: None,
        }
    }

    /// 在关闭流之后再执行该传输专属清理，避免清理动作与仍运行的命令竞争。
    pub(super) fn with_after_close(
        stream: Arc<Mutex<OperationsCommandStream>>,
        after_close: Arc<dyn ContainerPostStreamCleanup>,
    ) -> Self {
        Self {
            stream,
            after_close: Some(after_close),
        }
    }
}

/// 资源 owner 在原始命令关闭后执行的单次、幂等 staging 清理动作。
#[async_trait]
pub(super) trait ContainerPostStreamCleanup: Send + Sync {
    /// 使用清理队列给出的绝对 deadline，超时后由同一 owner 再次尝试。
    async fn cleanup_until(&self, deadline: tokio::time::Instant) -> AppResult<()>;
}

#[async_trait]
impl TransferResourceCleanup for ContainerStreamCleanupOwner {
    /// 按队列传入的绝对 deadline 重试关闭；超时后仍保留 stream 和 worker completion owner。
    async fn cleanup_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        let mut stream = self.stream.lock().await;
        stream.close_until(deadline).await?;
        drop(stream);
        if let Some(after_close) = &self.after_close {
            after_close.cleanup_until(deadline).await?;
        }
        Ok(())
    }
}

/// 流式操作错误携带清理所有权，避免把关闭超时伪装成资源已释放。
pub(super) struct ContainerStreamFailure {
    pub error: AppError,
    pub cleanup: TransferCleanupState,
    /// 仅表示输入未确认、且目标提交屏障尚未跨越；队列会先强制清理 owner 再允许显式重试。
    pub retryable_after_cleanup: bool,
}

impl ContainerStreamFailure {
    /// 尚未取得 stream 资源的失败明确标记 Released，避免为不存在的 worker 建空 owner。
    pub(super) fn released(error: AppError) -> Self {
        Self {
            error,
            cleanup: TransferCleanupState::Released,
            retryable_after_cleanup: false,
        }
    }

    /// 将目标提交前的输入回执缺失标为可在清理 owner 释放后重试。
    pub(super) fn retryable_after_cleanup(mut self) -> Self {
        self.retryable_after_cleanup = true;
        self
    }
}

/// 先释放流，再执行其私有 staging 清理；任一步失败都把仍可重试的 owner 交给队列。
pub(super) async fn close_failed_stream_with_cleanup(
    stream: Arc<Mutex<OperationsCommandStream>>,
    error: AppError,
    deadline: tokio::time::Instant,
    after_close: Option<Arc<dyn ContainerPostStreamCleanup>>,
) -> ContainerStreamFailure {
    let owner = match after_close {
        Some(after_close) => ContainerStreamCleanupOwner::with_after_close(stream, after_close),
        None => ContainerStreamCleanupOwner::new(stream),
    };
    match owner.cleanup_until(deadline).await {
        Ok(()) => ContainerStreamFailure::released(error),
        Err(cleanup_error) => ContainerStreamFailure {
            error: error.with_source(cleanup_error),
            cleanup: TransferCleanupState::pending(Arc::new(owner)),
            retryable_after_cleanup: false,
        },
    }
}

/// 保留主操作分类，并把流、进程或本机 worker 清理失败附加为可诊断 source。
pub(super) fn with_cleanup_error(primary: AppError, cleanup: AppResult<()>) -> AppError {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup_error) => {
            tracing::warn!(
                operation_error_code = primary.public_code().as_str(),
                cleanup_error_code = cleanup_error.public_code().as_str(),
                "容器传输收尾未完整确认"
            );
            primary.with_source(cleanup_error)
        }
    }
}

/// 已取得队列提交许可后，未收到成功回执的任何命令错误都必须标为 CommitUnknown。
pub(super) fn unknown_commit(destination: Option<FileEndpoint>) -> TransferExecutionError {
    TransferExecutionError::Failed(TransferExecutionFailure::new(
        TransferFailureKind::CommitUnknown,
        false,
        false,
        None,
        destination,
        TransferCleanupState::Released,
    ))
}

/// 检查远端源与请求类型完全一致；符号链接和其他对象在 tar 前 fail closed。
pub(super) fn validate_remote_source(
    object: RemoteObject,
    requested: TransferKind,
) -> AppResult<Option<u64>> {
    match (requested, object.kind) {
        (TransferKind::File, RemoteObjectKind::File) => Ok(object.size),
        (TransferKind::Directory, RemoteObjectKind::Directory) => Ok(None),
        (_, RemoteObjectKind::Symlink) => Err(AppError::unsupported("容器传输暂不跟随符号链接")),
        _ => Err(AppError::conflict("容器传输源类型与请求不匹配")),
    }
}

/// 返回最终操作目标中的 basename 和 parent，不把容器 path 按宿主平台解析。
pub(super) fn remote_parent_name(
    endpoint: &FileEndpoint,
) -> AppResult<(kerminal_domain::files::RemotePath, String)> {
    let target = container_endpoint(endpoint)?;
    let path = target.path.as_str();
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::invalid("容器传输目标必须是普通路径"))?
        .to_owned();
    validate_archive_name(&name)?;
    let parent = target
        .path
        .parent()
        .ok_or_else(|| AppError::invalid("容器传输目标缺少父目录"))?;
    Ok((parent, name))
}

/// 返回同一路径端点的顶层规范，不允许 retry 将本机目标切换成 SSH 或另一个容器。
pub(super) fn ensure_same_destination_kind(
    requested: &FileEndpoint,
    actual: &FileEndpoint,
) -> AppResult<()> {
    match (requested, actual) {
        (
            FileEndpoint::Container {
                host_id: left_host,
                runtime: left_runtime,
                container_id: left_id,
                ..
            },
            FileEndpoint::Container {
                host_id: right_host,
                runtime: right_runtime,
                container_id: right_id,
                ..
            },
        ) if left_host == right_host && left_runtime == right_runtime && left_id == right_id => {
            Ok(())
        }
        (FileEndpoint::Local { .. }, FileEndpoint::Local { .. }) => Ok(()),
        _ => Err(AppError::invalid("传输重试目标端点身份发生变化")),
    }
}

/// 将本机目标预检错误映射成安全固定类别，同时保留操作系统错误供内部诊断。
fn io_failure(context: &'static str, error: io::Error) -> AppError {
    match error.kind() {
        io::ErrorKind::NotFound => AppError::not_found(context),
        io::ErrorKind::AlreadyExists => AppError::conflict(context),
        _ => AppError::infrastructure_with_source(context, error),
    }
}
