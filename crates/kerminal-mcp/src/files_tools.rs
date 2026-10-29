//! MCP SFTP 传输工具到统一 TransferService 的边界映射。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_application::{AppError, AppResult, PublicErrorCode};
use kerminal_domain::{
    files::{
        ConflictPolicy, ContainerRuntime, FileEndpoint, RemotePath, TransferKind, TransferRequest,
        TransferStatus, TransferSummary,
    },
    identity::{HostId, TransferId},
};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args},
};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum TransferEndpointArgs {
    Local {
        path: String,
    },
    Remote {
        #[serde(rename = "hostId")]
        host_id: String,
        path: String,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TransferKindArgs {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConflictPolicyArgs {
    Overwrite,
    Skip,
    Rename,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TransferDirectionArgs {
    Upload,
    Download,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalTransferArgs {
    source: TransferEndpointArgs,
    destination: TransferEndpointArgs,
    kind: TransferKindArgs,
    conflict_policy: ConflictPolicyArgs,
    #[serde(default)]
    idle_timeout_seconds: Option<u16>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyTransferArgs {
    host_id: String,
    local_path: String,
    remote_path: String,
    direction: TransferDirectionArgs,
    kind: TransferKindArgs,
    conflict_policy: ConflictPolicyArgs,
    #[serde(default)]
    idle_timeout_seconds: Option<u16>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransferIdArgs {
    transfer_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OptionalTransferIdArgs {
    #[serde(default)]
    transfer_id: Option<String>,
}

/// 只把保留的传输工具路由到应用队列；此层不维护第二份连接或队列状态。
///
/// @author kongweiguang
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("SFTP MCP 请求已取消"));
    }
    match tool_id {
        "sftp.transfer.enqueue" => enqueue_transfer(services, arguments, cancellation).await,
        "sftp.transfer.list" => list_transfers(services, arguments, cancellation).await,
        "sftp.transfer.cancel" => cancel_transfer(services, arguments, cancellation).await,
        "sftp.transfer.retry" => retry_transfer(services, arguments, cancellation).await,
        _ => failure(AppError::unsupported("SFTP tool id is not supported")),
    }
}

/// 将新 canonical endpoint 或兼容的旧 flat 上传/下载请求一次性转成应用队列请求。
///
/// @author kongweiguang
async fn enqueue_transfer(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let request = match transfer_request(arguments) {
        Ok(request) => request,
        Err(error) => return failure(error),
    };
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("SFTP transfer enqueue was cancelled"));
    }
    let transfers = match services.require_transfers() {
        Ok(transfers) => transfers,
        Err(error) => return failure(error),
    };
    match transfers.enqueue(request).await {
        Ok(summary) => success_with_hints(
            "SFTP 传输任务已入队；这只表示受理，不代表文件已提交。".to_owned(),
            json!({ "transfer": transfer_projection(&summary) }),
            vec![transfer_entity(&summary)],
            vec!["使用 sftp.transfer.list 并传入 transferId 查询终态；只有 retryable=true 时才按用户意图调用 sftp.transfer.retry。".to_owned()],
        ),
        Err(error) => failure(error),
    }
}

/// 查询全队列或单个 SFTP 任务的权威快照，不依赖容易丢帧的广播事件。
///
/// @author kongweiguang
async fn list_transfers(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<OptionalTransferIdArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("SFTP transfer query was cancelled"));
    }
    let transfer_id = match args.transfer_id.as_deref() {
        Some(value) if value.trim().is_empty() => {
            return failure(AppError::invalid("transferId cannot be empty"));
        }
        Some(value) => match TransferId::parse(value.trim()) {
            Ok(id) => Some(id),
            Err(_) => return failure(AppError::invalid("transfer id")),
        },
        None => None,
    };
    let transfers = match services.require_transfers() {
        Ok(transfers) => transfers,
        Err(error) => return failure(error),
    };
    let all = transfers.list().await;
    let selected = all
        .into_iter()
        .filter(|summary| transfer_id.as_ref().is_none_or(|id| &summary.id == id))
        .collect::<Vec<_>>();
    let projections = selected.iter().map(transfer_projection).collect::<Vec<_>>();
    let entities = selected.iter().map(transfer_entity).collect::<Vec<_>>();
    let count = selected.len();
    let summary = match transfer_id.as_ref() {
        Some(id) if count == 0 => format!("未找到 SFTP 传输任务：{id}。"),
        Some(id) => format!("已找到 SFTP 传输任务：{id}。"),
        None if count == 0 => "当前没有 SFTP 传输任务。".to_owned(),
        None => format!("当前共有 {count} 个 SFTP 传输任务。"),
    };
    let next_hints = transfer_list_hints(&selected);
    success_with_hints(
        summary,
        json!({ "transfers": projections, "count": count, "transferId": transfer_id.as_ref().map(TransferId::as_str) }),
        entities,
        next_hints,
    )
}

/// 请求取消任务并返回当下状态；提交阶段由 TransferService 明确拒绝取消。
///
/// @author kongweiguang
async fn cancel_transfer(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<TransferIdArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled(
            "SFTP transfer cancellation was cancelled",
        ));
    }
    let id = match TransferId::parse(&args.transfer_id) {
        Ok(id) => id,
        Err(_) => return failure(AppError::invalid("transfer id")),
    };
    let transfers = match services.require_transfers() {
        Ok(transfers) => transfers,
        Err(error) => return failure(error),
    };
    match transfers.cancel(&id).await {
        Ok(summary) => {
            let projection = transfer_projection(&summary);
            success_with_hints(
                format!("已处理 SFTP 传输任务取消请求：{}。", summary.id),
                json!({ "transfer": projection }),
                vec![transfer_entity(&summary)],
                vec!["取消请求只表示队列已处理该请求；请继续使用 sftp.transfer.list 查询最终状态。提交中的任务不能取消。".to_owned()],
            )
        }
        Err(error) => failure(error),
    }
}

/// 按原任务身份创建唯一后继任务，重试请求不接受新的路径或冲突策略。
///
/// @author kongweiguang
async fn retry_transfer(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<TransferIdArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("SFTP transfer retry was cancelled"));
    }
    let id = match TransferId::parse(&args.transfer_id) {
        Ok(id) => id,
        Err(_) => return failure(AppError::invalid("transfer id")),
    };
    let transfers = match services.require_transfers() {
        Ok(transfers) => transfers,
        Err(error) => return failure(error),
    };
    match transfers.retry(&id).await {
        Ok(summary) => {
            let completed = matches!(
                summary.status,
                TransferStatus::Succeeded | TransferStatus::Unchanged
            );
            success_with_hints(
                format!("SFTP 重试请求已接受：{}。", summary.id),
                json!({
                    "transfer": transfer_projection(&summary),
                    "retry": {
                        "sourceTransferId": id.as_str(),
                        "transferId": summary.id.as_str(),
                        "accepted": true,
                        "completed": completed,
                        "resumeMode": if summary.resumable { "pendingValidation" } else { "fromStart" }
                    }
                }),
                vec![transfer_entity(&summary)],
                vec![
                    "使用 retry.transferId 调用 sftp.transfer.list 跟踪后继任务，直到观察到终态。"
                        .to_owned(),
                ],
            )
        }
        Err(error) => retry_failure(error),
    }
}

/// 仅在重试拒绝因提交结果未知时映射旧版专用错误族，不改变其他未知结果错误。
///
/// @author kongweiguang
fn retry_failure(error: AppError) -> CallToolResult {
    if error.public_code() != PublicErrorCode::OutcomeUnknown {
        return failure(error);
    }
    let message =
        "该传输任务暂不能继续；请按 transferId 查询最新状态，核对 retryable 和 successorId。";
    CallToolResult::structured_error(json!({
        "summary": message,
        "error": message,
        "data": {},
        "entities": [],
        "errorKind": "sftpTransferRetryUnavailable",
        "recoverable": false,
        "nextHints": ["调用 sftp.transfer.list({transferId}) 查询权威任务状态；提交结果未确认时先核对目标文件，不能直接重试。"]
    }))
}

/// 按旧工具契约支持 canonical 端点和已投入使用的 flat upload/download 请求。
///
/// @author kongweiguang
fn transfer_request(arguments: Map<String, Value>) -> AppResult<TransferRequest> {
    let has_canonical = arguments.contains_key("source") || arguments.contains_key("destination");
    let has_legacy = ["hostId", "localPath", "remotePath", "direction"]
        .iter()
        .any(|key| arguments.contains_key(*key));
    if has_canonical && has_legacy {
        return Err(AppError::invalid(
            "sftp.transfer.enqueue cannot mix canonical endpoints and legacy fields",
        ));
    }
    if has_canonical {
        let args = parse_args::<CanonicalTransferArgs>(arguments)?;
        validate_idle_timeout(args.idle_timeout_seconds)?;
        let source = transfer_endpoint(args.source, "source")?;
        let destination = transfer_endpoint(args.destination, "destination")?;
        let kind = transfer_kind(args.kind);
        validate_directory_transfer(&source, &destination, kind)?;
        return Ok(TransferRequest {
            source,
            destination,
            kind,
            conflict_policy: conflict_policy(args.conflict_policy),
            idle_timeout_seconds: args.idle_timeout_seconds,
        });
    }

    let args = parse_args::<LegacyTransferArgs>(arguments)?;
    validate_idle_timeout(args.idle_timeout_seconds)?;
    let local = local_endpoint(&args.local_path, "localPath")?;
    let remote = remote_endpoint(&args.host_id, &args.remote_path)?;
    let (source, destination) = match args.direction {
        TransferDirectionArgs::Upload => (local, remote),
        TransferDirectionArgs::Download => (remote, local),
    };
    let kind = transfer_kind(args.kind);
    validate_directory_transfer(&source, &destination, kind)?;
    Ok(TransferRequest {
        source,
        destination,
        kind,
        conflict_policy: conflict_policy(args.conflict_policy),
        idle_timeout_seconds: args.idle_timeout_seconds,
    })
}

/// 把外部 endpoint 转成不含凭据的领域目标，并阻止远程根目录作为复制端点。
///
/// @author kongweiguang
fn transfer_endpoint(args: TransferEndpointArgs, label: &str) -> AppResult<FileEndpoint> {
    match args {
        TransferEndpointArgs::Local { path } => local_endpoint(&path, label),
        TransferEndpointArgs::Remote { host_id, path } => remote_endpoint(&host_id, &path),
    }
}

/// 校验本机路径文本并保留其原生 Windows/Unix 分隔符语义。
///
/// @author kongweiguang
fn local_endpoint(path: &str, label: &str) -> AppResult<FileEndpoint> {
    let path = path.trim();
    if path.is_empty() || path.chars().any(char::is_control) {
        tracing::debug!(label, "invalid local path in SFTP transfer request");
        return Err(AppError::invalid("local path is invalid"));
    }
    Ok(FileEndpoint::Local {
        path: PathBuf::from(path),
    })
}

/// 将 opaque host id 和 POSIX 路径解析为受限传输端点，禁止把远程根目录作为复制目标。
///
/// @author kongweiguang
fn remote_endpoint(host_id: &str, path: &str) -> AppResult<FileEndpoint> {
    let host_id = HostId::parse(host_id).map_err(|_| AppError::invalid("host id"))?;
    let normalized_path = path.replace('\\', "/");
    let path = RemotePath::parse(&normalized_path).map_err(|_| AppError::invalid("remote path"))?;
    if path.as_str() == "/" {
        return Err(AppError::invalid("remote root is not a file target"));
    }
    Ok(FileEndpoint::Remote { host_id, path })
}

/// 拒绝目录传输源与目标互为祖先，避免递归复制把一侧再次纳入另一侧。
///
/// @author kongweiguang
fn validate_directory_transfer(
    source: &FileEndpoint,
    destination: &FileEndpoint,
    kind: TransferKind,
) -> AppResult<()> {
    if kind != TransferKind::Directory {
        return Ok(());
    }
    match (source, destination) {
        (
            FileEndpoint::Remote {
                host_id: source_host,
                path: source_path,
            },
            FileEndpoint::Remote {
                host_id: target_host,
                path: target_path,
            },
        ) if source_host == target_host => {
            let source = source_path.as_str();
            let destination = target_path.as_str();
            if remote_path_contains(source, destination)
                || remote_path_contains(destination, source)
            {
                return Err(AppError::invalid(
                    "same-host directory transfer cannot nest source and destination",
                ));
            }
        }
        (FileEndpoint::Local { path: source }, FileEndpoint::Local { path: destination })
            if source.starts_with(destination) || destination.starts_with(source) =>
        {
            return Err(AppError::invalid(
                "local directory transfer cannot nest source and destination",
            ));
        }
        _ => {}
    }
    Ok(())
}

/// 用 POSIX 路径分隔边界比较远端目录祖先关系，不解释 Windows 盘符语义。
///
/// @author kongweiguang
fn remote_path_contains(parent: &str, candidate: &str) -> bool {
    if parent == candidate {
        return true;
    }
    if parent == "." {
        return !candidate.starts_with('/') && !candidate.starts_with("../");
    }
    let parent = parent.trim_end_matches('/');
    if parent.is_empty() {
        return candidate.starts_with('/') && candidate != "/";
    }
    candidate
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

/// 将用户冲突策略映射到领域队列，不在 MCP 自己挑选 rename 目标或实现 skip。
///
/// @author kongweiguang
fn conflict_policy(policy: ConflictPolicyArgs) -> ConflictPolicy {
    match policy {
        ConflictPolicyArgs::Overwrite => ConflictPolicy::Overwrite,
        ConflictPolicyArgs::Skip => ConflictPolicy::Skip,
        ConflictPolicyArgs::Rename => ConflictPolicy::Rename,
    }
}

/// 将外部文件/目录标记映射为队列领域类型。
///
/// @author kongweiguang
fn transfer_kind(kind: TransferKindArgs) -> TransferKind {
    match kind {
        TransferKindArgs::File => TransferKind::File,
        TransferKindArgs::Directory => TransferKind::Directory,
    }
}

/// 入队处统一限制可覆盖的 idle 阈值，防止工具输入产生无界后台任务。
///
/// @author kongweiguang
fn validate_idle_timeout(seconds: Option<u16>) -> AppResult<()> {
    if seconds.is_some_and(|value| !(30..=3_600).contains(&value)) {
        return Err(AppError::invalid(
            "idleTimeoutSeconds must be between 30 and 3600",
        ));
    }
    Ok(())
}

/// 为外部查询投影传输摘要，不泄露后端错误、暂存路径或主机配置标签。
///
/// @author kongweiguang
fn transfer_projection(summary: &TransferSummary) -> Value {
    let request = &summary.request;
    let destination = summary
        .actual_destination
        .as_ref()
        .unwrap_or(&request.destination);
    let operation = transfer_operation(&request.source, destination);
    let failure_kind = summary.failure_kind.map(transfer_failure_kind_value);
    let mut projection = json!({
        "id": summary.id.as_str(),
        "status": transfer_status_value(summary.status),
        "operation": operation,
        "source": transfer_endpoint_value(&request.source),
        "destination": transfer_endpoint_value(destination),
        "kind": match request.kind { TransferKind::File => "file", TransferKind::Directory => "directory" },
        "conflictPolicy": conflict_policy_value(request.conflict_policy),
        "progress": {
            "bytesTransferred": summary.bytes_transferred,
            "totalBytes": summary.total_bytes,
            "speedBytesPerSecond": summary.speed_bytes_per_second
        },
        "idleTimeoutSeconds": summary.effective_idle_timeout_seconds,
        "cancelRequested": matches!(summary.status, TransferStatus::Cancelling | TransferStatus::Cancelled),
        "lastProgressAt": summary.last_progress_at,
        "recoveryAttempt": u8::from(summary.automatic_recovery_used),
        "retryable": summary.retryable,
        "resumable": summary.resumable,
        "successorId": summary.retry_successor.as_ref().map(TransferId::as_str),
        "failureKind": failure_kind,
        "failure": summary.failure_kind.map(|kind| json!({
            "kind": transfer_failure_kind_value(kind),
            "idleTimeoutSeconds": summary.effective_idle_timeout_seconds,
            "bytesTransferred": summary.bytes_transferred,
            "retryable": summary.retryable,
            "resumable": summary.resumable
        })),
        "transportMode": if matches!(request.source, FileEndpoint::Container { .. }) || matches!(destination, FileEndpoint::Container { .. }) { "containerArchive" } else if matches!(request.source, FileEndpoint::Remote { .. }) && matches!(destination, FileEndpoint::Remote { .. }) { "clientBridge" } else { "singleHostSftp" },
        "phase": transfer_phase_value(summary.status),
        "error": summary.safe_error
    });
    if summary.cleanup_pending {
        projection["cleanupPending"] = json!(true);
    }
    if summary.status == TransferStatus::Unchanged {
        projection["commitSummary"] = json!({
            "outcome": "unchanged",
            "committedEntries": summary.commit_summary.committed_entries(),
            "committedBytes": summary.commit_summary.committed_bytes()
        });
    } else if summary.partial_commit {
        projection["commitSummary"] = json!({
            "outcome": "partial",
            "committedEntries": summary.commit_summary.committed_entries(),
            "committedBytes": summary.commit_summary.committed_bytes()
        });
    }
    projection
}

/// 生成统一 sftpTransfer entity，便于调用方按任务 id 合并队列状态。
///
/// @author kongweiguang
fn transfer_entity(summary: &TransferSummary) -> Value {
    json!({
        "type": "sftpTransfer",
        "id": summary.id.as_str(),
        "transfer": transfer_projection(summary)
    })
}

/// 队列也承载容器归档传输，必须保留可回读的目标身份，不能把已受理的容器任务标成 unsupported。
///
/// @author kongweiguang
fn transfer_endpoint_value(endpoint: &FileEndpoint) -> Value {
    match endpoint {
        FileEndpoint::Local { path } => json!({
            "type": "local",
            "path": path.to_string_lossy()
        }),
        FileEndpoint::Remote { host_id, path } => json!({
            "type": "remote",
            "hostId": host_id.as_str(),
            "path": path.as_str()
        }),
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => json!({
            "type": "container",
            "hostId": host_id.as_ref().map(HostId::as_str),
            "runtime": if *runtime == ContainerRuntime::Docker { "docker" } else { "podman" },
            "containerId": container_id,
            "path": path.as_str()
        }),
    }
}

/// 按本机与远端（含容器）之间的方向报告上传下载，保持普通 SFTP 的旧值并避免容器上传误报 remoteCopy。
///
/// @author kongweiguang
fn transfer_operation(source: &FileEndpoint, destination: &FileEndpoint) -> &'static str {
    match (source, destination) {
        (
            FileEndpoint::Local { .. },
            FileEndpoint::Remote { .. } | FileEndpoint::Container { .. },
        ) => "upload",
        (
            FileEndpoint::Remote { .. } | FileEndpoint::Container { .. },
            FileEndpoint::Local { .. },
        ) => "download",
        (FileEndpoint::Remote { .. }, FileEndpoint::Remote { .. }) => "remoteCopy",
        _ => "remoteCopy",
    }
}

/// 映射领域任务生命周期到稳定 MCP 状态文本，保留内部 Skipped 的明确含义。
///
/// @author kongweiguang
fn transfer_status_value(status: TransferStatus) -> &'static str {
    match status {
        TransferStatus::Queued => "queued",
        TransferStatus::Connecting
        | TransferStatus::Transferring
        | TransferStatus::WaitingForRecovery
        | TransferStatus::Committing
        | TransferStatus::Cancelling => "running",
        TransferStatus::Succeeded => "succeeded",
        TransferStatus::Unchanged => "succeeded",
        TransferStatus::Skipped => "skipped",
        TransferStatus::Failed => "failed",
        TransferStatus::Cancelled => "canceled",
    }
}

/// 为兼容旧协议把内部提交、恢复、取消阶段压成稳定短标签。
///
/// @author kongweiguang
fn transfer_phase_value(status: TransferStatus) -> &'static str {
    match status {
        TransferStatus::Queued => "queued",
        TransferStatus::Connecting => "connecting",
        TransferStatus::Transferring => "transferring",
        TransferStatus::WaitingForRecovery => "waiting",
        TransferStatus::Committing => "committing",
        TransferStatus::Cancelling => "cancelling",
        TransferStatus::Succeeded => "succeeded",
        TransferStatus::Unchanged => "succeeded",
        TransferStatus::Skipped => "skipped",
        TransferStatus::Failed => "failed",
        TransferStatus::Cancelled => "canceled",
    }
}

/// 保持失败恢复类别稳定，不把易变底层错误文本写入 MCP 返回值。
///
/// @author kongweiguang
fn transfer_failure_kind_value(kind: kerminal_domain::files::TransferFailureKind) -> &'static str {
    use kerminal_domain::files::TransferFailureKind;
    match kind {
        TransferFailureKind::IdleTimeout => "idleTimeout",
        TransferFailureKind::CommitUnknown => "commitUnknown",
        TransferFailureKind::Connection
        | TransferFailureKind::Permission
        | TransferFailureKind::Conflict
        | TransferFailureKind::InvalidSource
        | TransferFailureKind::Unsupported
        | TransferFailureKind::Internal => "other",
    }
}

/// 映射冲突策略到小写历史 wire 值。
///
/// @author kongweiguang
fn conflict_policy_value(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Overwrite => "overwrite",
        ConflictPolicy::Skip => "skip",
        ConflictPolicy::Rename => "rename",
    }
}

/// 根据失败类别、可重试性和后继关系返回可执行的下一步提示。
///
/// @author kongweiguang
fn transfer_list_hints(transfers: &[TransferSummary]) -> Vec<String> {
    if transfers.iter().any(|task| {
        task.failure_kind == Some(kerminal_domain::files::TransferFailureKind::CommitUnknown)
    }) {
        return vec!["提交结果未确认：先检查目标文件，不要自动重试或再次上传。".to_owned()];
    }
    if transfers
        .iter()
        .any(|task| task.retryable && task.retry_successor.is_none())
    {
        return vec![
            "任务可安全重试时，使用 sftp.transfer.retry 并继续查询其返回的后继 id。".to_owned(),
        ];
    }
    if transfers.iter().any(|task| {
        matches!(
            task.status,
            TransferStatus::Queued
                | TransferStatus::Connecting
                | TransferStatus::Transferring
                | TransferStatus::WaitingForRecovery
                | TransferStatus::Committing
                | TransferStatus::Cancelling
        )
    }) {
        return vec![
            "传输仍在后台运行；稍后按 transferId 再次查询，按用户要求决定是否取消。".to_owned(),
        ];
    }
    if transfers
        .iter()
        .any(|task| task.status == TransferStatus::Skipped)
    {
        return vec!["冲突策略已跳过目标；该任务没有写入目标文件。".to_owned()];
    }
    Vec::new()
}

/// 生成需要额外恢复提示的结构化成功结果，保持共享 helper 的标准顶层字段。
///
/// @author kongweiguang
fn success_with_hints(
    summary: String,
    data: Value,
    entities: Vec<Value>,
    next_hints: Vec<String>,
) -> CallToolResult {
    CallToolResult::structured(json!({
        "summary": summary,
        "data": data,
        "entities": entities,
        "nextHints": next_hints
    }))
}

#[cfg(test)]
#[path = "../tests/support/files_tools_unit.rs"]
mod tests;
