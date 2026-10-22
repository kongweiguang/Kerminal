//! MCP SFTP 工具到统一 FileService 与 TransferService 的边界映射。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_application::{AppError, AppResult, PublicErrorCode, files::MAX_FILE_PREVIEW_BYTES};
use kerminal_domain::{
    files::{
        ConflictPolicy, DeleteTargetKind, FileEndpoint, FileEntry, FileEntryKind, FilePreview,
        RemotePath, TransferKind, TransferRequest, TransferStatus, TransferSummary,
    },
    identity::{HostId, TransferId},
};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};

const DEFAULT_PREVIEW_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostPathArgs {
    host_id: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenameArgs {
    host_id: String,
    from_path: String,
    to_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreviewArgs {
    host_id: String,
    path: String,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteArgs {
    host_id: String,
    path: String,
    #[serde(default)]
    directory: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChmodArgs {
    host_id: String,
    path: String,
    mode: String,
}

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

/// 只把冻结的 SFTP tool id 路由到 application 用例；此层不维护第二份连接或队列状态。
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
        "sftp.list" => list(services, arguments, cancellation).await,
        "sftp.rename" | "sftp.move" => {
            rename_file(services, arguments, cancellation, tool_id == "sftp.move").await
        }
        "sftp.preview" => preview(services, arguments, cancellation).await,
        "sftp.delete" => delete(services, arguments, cancellation).await,
        "sftp.create_directory" => create_directory(services, arguments, cancellation).await,
        "sftp.chmod" => chmod(services, arguments, cancellation).await,
        "sftp.transfer.enqueue" => enqueue_transfer(services, arguments, cancellation).await,
        "sftp.transfer.list" => list_transfers(services, arguments, cancellation).await,
        "sftp.transfer.cancel" => cancel_transfer(services, arguments, cancellation).await,
        "sftp.transfer.retry" => retry_transfer(services, arguments, cancellation).await,
        "sftp.transfer.clear_completed" => clear_completed(services, cancellation).await,
        _ => failure(AppError::unsupported("SFTP tool id is not supported")),
    }
}

/// 把 rename 与 move 的旧工具 ID 映射到同一文件用例，并保持远端路径显式校验。
///
/// @author kongweiguang
async fn rename_file(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
    move_alias: bool,
) -> CallToolResult {
    let args = match parse_args::<RenameArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let source = match remote_endpoint(&args.host_id, &args.from_path, false) {
        Ok(endpoint) => endpoint,
        Err(error) => return failure(error),
    };
    let destination = match remote_endpoint(&args.host_id, &args.to_path, false) {
        Ok(endpoint) => endpoint,
        Err(error) => return failure(error),
    };
    let files = match services.require_files() {
        Ok(files) => files,
        Err(error) => return failure(error),
    };

    match files.rename(cancellation, source, destination).await {
        Ok(()) => success(
            if move_alias {
                format!("远程路径已移动：{} -> {}。", args.from_path, args.to_path)
            } else {
                format!("远程路径已重命名：{} -> {}。", args.from_path, args.to_path)
            },
            json!({
                "hostId": args.host_id,
                "fromPath": args.from_path,
                "toPath": args.to_path
            }),
            Vec::new(),
        ),
        Err(error) => failure(error),
    }
}

/// 读取一层远端目录并沿用外部 Agent 已使用的 entry 与 entity 形状。
///
/// @author kongweiguang
async fn list(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<HostPathArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let endpoint = match remote_endpoint(&args.host_id, &args.path, true) {
        Ok(endpoint) => endpoint,
        Err(error) => return failure(error),
    };
    let listing = match services.require_files() {
        Ok(files) => match files.list_directory(cancellation, endpoint).await {
            Ok(listing) => listing,
            Err(error) => return failure(error),
        },
        Err(error) => return failure(error),
    };
    let (host_id, path) = match remote_endpoint_parts(&listing.directory) {
        Ok(parts) => parts,
        Err(error) => return failure(error),
    };
    let mut entries = Vec::with_capacity(listing.entries.len());
    let mut entities = Vec::with_capacity(listing.entries.len());
    for entry in &listing.entries {
        let value = match file_entry_value(entry) {
            Ok(value) => value,
            Err(error) => return failure(error),
        };
        entities.push(json!({
            "type": "sftpEntry",
            "hostId": host_id,
            "path": path,
            "name": entry.name,
            "kind": entry_kind_value(entry.kind),
            "size": entry.size,
            "permissions": entry.permissions.map(|mode| permission_string(entry.kind, mode)),
            "modified": entry.modified,
            "raw": entry.raw,
        }));
        entries.push(value);
    }
    let count = entries.len();
    success(
        format!("远程目录已读取：{host_id}:{path}，共 {count} 项。"),
        json!({ "hostId": host_id, "path": path, "entryCount": count, "entries": entries }),
        entities,
    )
}

/// 预览严格受应用层最大字节预算约束，二进制正文不会被冒充为文本交给 Agent。
///
/// @author kongweiguang
async fn preview(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<PreviewArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let endpoint = match remote_endpoint(&args.host_id, &args.path, false) {
        Ok(endpoint) => endpoint,
        Err(error) => return failure(error),
    };
    let max_bytes = args
        .max_bytes
        .unwrap_or(DEFAULT_PREVIEW_BYTES)
        .min(MAX_FILE_PREVIEW_BYTES);
    if max_bytes == 0 {
        return failure(AppError::invalid("maxBytes must be greater than zero"));
    }
    let files = match services.require_files() {
        Ok(files) => files,
        Err(error) => return failure(error),
    };
    let preview = match files.preview_file(cancellation, endpoint, max_bytes).await {
        Ok(preview) => preview,
        Err(error) => return failure(error),
    };
    let (host_id, path) = match remote_endpoint_parts(&preview.endpoint) {
        Ok(parts) => parts,
        Err(error) => return failure(error),
    };
    let content = preview_content(&preview);
    let encoding = if preview.binary { "binary" } else { "utf-8" };
    success(
        format!(
            "远程文件已预览：{host_id}:{path}，读取 {} / {max_bytes} 字节{}。",
            preview.bytes.len(),
            if preview.truncated {
                "，内容已截断"
            } else {
                ""
            }
        ),
        json!({
            "hostId": host_id,
            "path": path,
            "content": content,
            "bytesRead": preview.bytes.len(),
            "maxBytes": max_bytes,
            "truncated": preview.truncated,
            "encoding": encoding
        }),
        Vec::new(),
    )
}

/// 通过远端 adapter 删除文件或仅删除空目录，不把旧 SFTP 语义扩展成递归删除。
///
/// @author kongweiguang
async fn delete(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<DeleteArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let endpoint = match remote_endpoint(&args.host_id, &args.path, false) {
        Ok(endpoint) => endpoint,
        Err(error) => return failure(error),
    };
    let kind = if args.directory.unwrap_or(false) {
        DeleteTargetKind::Directory
    } else {
        DeleteTargetKind::File
    };
    let files = match services.require_files() {
        Ok(files) => files,
        Err(error) => return failure(error),
    };
    match files.delete(cancellation, endpoint, kind).await {
        Ok(()) => success(
            if kind == DeleteTargetKind::Directory {
                format!("远程空目录删除已执行：{}:{}。", args.host_id, args.path)
            } else {
                format!("远程文件删除已执行：{}:{}。", args.host_id, args.path)
            },
            json!({ "hostId": args.host_id, "path": args.path, "directory": kind == DeleteTargetKind::Directory }),
            Vec::new(),
        ),
        Err(error) => failure(error),
    }
}

/// 新建单层目录并把创建结果返回给 MCP host。
///
/// @author kongweiguang
async fn create_directory(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<HostPathArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let endpoint = match remote_endpoint(&args.host_id, &args.path, false) {
        Ok(endpoint) => endpoint,
        Err(error) => return failure(error),
    };
    let files = match services.require_files() {
        Ok(files) => files,
        Err(error) => return failure(error),
    };
    match files.create_directory(cancellation, endpoint).await {
        Ok(()) => success(
            format!("远程目录已创建：{}:{}。", args.host_id, args.path),
            json!({ "hostId": args.host_id, "path": args.path }),
            Vec::new(),
        ),
        Err(error) => failure(error),
    }
}

/// 解析八进制权限文本后调用 SFTP chmod，禁止接受任意非八进制字符。
///
/// @author kongweiguang
async fn chmod(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<ChmodArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let endpoint = match remote_endpoint(&args.host_id, &args.path, false) {
        Ok(endpoint) => endpoint,
        Err(error) => return failure(error),
    };
    let mode_text = args.mode.trim();
    if mode_text.is_empty()
        || mode_text.len() > 4
        || !mode_text.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
    {
        return failure(AppError::invalid(
            "mode must be a three or four digit octal value",
        ));
    }
    let mode = match u32::from_str_radix(mode_text, 8) {
        Ok(mode) if mode <= 0o7777 => mode,
        _ => {
            return failure(AppError::invalid(
                "mode is outside supported Unix permissions",
            ));
        }
    };
    let files = match services.require_files() {
        Ok(files) => files,
        Err(error) => return failure(error),
    };
    match files.chmod(cancellation, endpoint, mode).await {
        Ok(()) => success(
            format!(
                "远程路径权限已修改：{}:{} -> {:o}。",
                args.host_id, args.path, mode
            ),
            json!({ "hostId": args.host_id, "path": args.path, "mode": format!("{:o}", mode) }),
            Vec::new(),
        ),
        Err(error) => failure(error),
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

/// 清理可安全移除的终态记录，并把 MCP host 的运行中取消传递给队列用例。
///
/// @author kongweiguang
async fn clear_completed(
    services: &McpServices,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("SFTP transfer cleanup was cancelled"));
    }
    let transfers = match services.require_transfers() {
        Ok(transfers) => transfers,
        Err(error) => return failure(error),
    };
    match transfers.clear_completed(cancellation).await {
        Ok((removed_count, remaining_count)) => success(
            format!(
                "已清理 {removed_count} 个可安全移除的结束任务，当前保留 {remaining_count} 个任务；仍需核对的提交结果会保留。"
            ),
            json!({ "removedCount": removed_count, "remainingCount": remaining_count }),
            Vec::new(),
        ),
        Err(error) => failure(error),
    }
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
    let remote = remote_endpoint(&args.host_id, &args.remote_path, false)?;
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
        TransferEndpointArgs::Remote { host_id, path } => remote_endpoint(&host_id, &path, false),
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

/// 将 opaque host id 和 POSIX 路径解析为受限 SFTP endpoint。
///
/// @author kongweiguang
fn remote_endpoint(host_id: &str, path: &str, allow_root: bool) -> AppResult<FileEndpoint> {
    let host_id = HostId::parse(host_id).map_err(|_| AppError::invalid("host id"))?;
    let normalized_path = path.replace('\\', "/");
    let path = RemotePath::parse(&normalized_path).map_err(|_| AppError::invalid("remote path"))?;
    if !allow_root && path.as_str() == "/" {
        return Err(AppError::invalid("remote root is not a file target"));
    }
    Ok(FileEndpoint::Remote { host_id, path })
}

/// 读取已构造的远端 endpoint 标识，用于输出外部协议的 hostId/path 字段。
///
/// @author kongweiguang
fn remote_endpoint_parts(endpoint: &FileEndpoint) -> AppResult<(&str, &str)> {
    match endpoint {
        FileEndpoint::Remote { host_id, path } => Ok((host_id.as_str(), path.as_str())),
        _ => Err(AppError::infrastructure(
            "SFTP adapter returned a non-remote endpoint",
        )),
    }
}

/// 转换单个领域目录行；没有协议原文或属性投影时以 null 保留旧 wire 字段形状。
///
/// @author kongweiguang
fn file_entry_value(entry: &FileEntry) -> AppResult<Value> {
    let path = match &entry.endpoint {
        FileEndpoint::Remote { path, .. } => path.as_str().to_owned(),
        _ => {
            return Err(AppError::infrastructure(
                "SFTP entry returned a non-remote endpoint",
            ));
        }
    };
    Ok(json!({
        "name": entry.name,
        "path": path,
        "kind": entry_kind_value(entry.kind),
        "size": entry.size,
        "permissions": entry.permissions.map(|mode| permission_string(entry.kind, mode)),
        "modified": entry.modified,
        "raw": entry.raw,
    }))
}

/// 保持历史 MCP 目录项类型的小写 wire 值。
///
/// @author kongweiguang
fn entry_kind_value(kind: FileEntryKind) -> &'static str {
    match kind {
        FileEntryKind::File => "file",
        FileEntryKind::Directory => "directory",
        FileEntryKind::Symlink => "symlink",
        FileEntryKind::Other => "other",
    }
}

/// 将文件类型和 Unix mode 格式化为十位 ls 权限文本；特殊位未设置时仍显示普通 `x` 或 `-`。
///
/// @author kongweiguang
fn permission_string(kind: FileEntryKind, mode: u32) -> String {
    let file_type = match kind {
        FileEntryKind::File => '-',
        FileEntryKind::Directory => 'd',
        FileEntryKind::Symlink => 'l',
        FileEntryKind::Other => '?',
    };
    let mut output = String::with_capacity(10);
    output.push(file_type);
    for (read, write, execute) in [
        (0o400, 0o200, 0o100),
        (0o040, 0o020, 0o010),
        (0o004, 0o002, 0o001),
    ] {
        output.push(if mode & read != 0 { 'r' } else { '-' });
        output.push(if mode & write != 0 { 'w' } else { '-' });
        let special = match execute {
            0o100 => 0o4000,
            0o010 => 0o2000,
            _ => 0o1000,
        };
        let marker = if mode & special != 0 {
            if special == 0o1000 {
                if mode & execute != 0 { 't' } else { 'T' }
            } else if mode & execute != 0 {
                's'
            } else {
                'S'
            }
        } else if mode & execute != 0 {
            'x'
        } else {
            '-'
        };
        output.push(marker);
    }
    output
}

/// 只对预览正文做 UTF-8 解码；截断末尾的不完整码点被裁掉而不被误标为二进制。
///
/// @author kongweiguang
fn preview_content(preview: &FilePreview) -> String {
    if preview.binary {
        return String::new();
    }
    match std::str::from_utf8(&preview.bytes) {
        Ok(text) => text.to_owned(),
        Err(error) if preview.truncated && error.error_len().is_none() => {
            String::from_utf8_lossy(&preview.bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(&preview.bytes).into_owned(),
    }
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
        "transportMode": if matches!(request.source, FileEndpoint::Remote { .. }) && matches!(destination, FileEndpoint::Remote { .. }) { "clientBridge" } else { "singleHostSftp" },
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

/// 输出统一 endpoint wire shape，local path 使用宿主平台原生文本。
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
        FileEndpoint::Container { .. } => json!({ "type": "unsupported" }),
    }
}

/// 根据实际端点方向确定旧 transfer projection 的 operation 字段。
///
/// @author kongweiguang
fn transfer_operation(source: &FileEndpoint, destination: &FileEndpoint) -> &'static str {
    match (source, destination) {
        (FileEndpoint::Local { .. }, FileEndpoint::Remote { .. }) => "upload",
        (FileEndpoint::Remote { .. }, FileEndpoint::Local { .. }) => "download",
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
