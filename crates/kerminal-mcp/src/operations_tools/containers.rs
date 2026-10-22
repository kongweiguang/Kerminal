//! 容器运行时与容器文件 MCP 工具到 application use cases 的映射。
//!
//! @author kongweiguang

use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use kerminal_application::{AppError, files::MAX_FILE_PREVIEW_BYTES};
use kerminal_domain::{
    files::{
        ConflictPolicy, ContainerRuntime, DeleteTargetKind, FileEndpoint, FileRevision, RemotePath,
        TransferKind, TransferRequest, WriteTextRequest, WriteTextStrategy,
    },
    identity::HostId,
    operations::{ContainerAction, ContainerListQuery, ContainerTarget},
};
use rmcp::model::CallToolResult;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};
mod arguments;
use arguments::{
    ContainerChmodArgs, ContainerDeleteArgs, ContainerListArgs, ContainerLogsArgs,
    ContainerPathArgs, ContainerPreviewArgs, ContainerRemoveArgs, ContainerRenameArgs,
    ContainerTargetArgs, ContainerTransferArgs, ContainerWriteArgs, RevisionArgs,
};

/// 显式分派容器工具，不让单一 handler 把容器生命周期和文件策略混在一起。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    match tool_id {
        "container.list" => list(services, arguments, cancellation).await,
        "container.inspect" => inspect(services, arguments, cancellation).await,
        "container.logs.tail" => logs(services, arguments, cancellation).await,
        "container.stats" => stats(services, arguments, cancellation).await,
        "container.start" => {
            mutate(services, arguments, cancellation, ContainerAction::Start).await
        }
        "container.stop" => mutate(services, arguments, cancellation, ContainerAction::Stop).await,
        "container.restart" => {
            mutate(services, arguments, cancellation, ContainerAction::Restart).await
        }
        "container.remove" => {
            mutate(services, arguments, cancellation, ContainerAction::Remove).await
        }
        "container.files.list" => file_list(services, arguments, cancellation).await,
        "container.files.preview" => file_preview(services, arguments, cancellation).await,
        "container.files.write_text" => file_write(services, arguments, cancellation).await,
        "container.files.create_directory" => {
            file_create_directory(services, arguments, cancellation).await
        }
        "container.files.rename" => file_rename(services, arguments, cancellation).await,
        "container.files.chmod" => file_chmod(services, arguments, cancellation).await,
        "container.files.delete" => file_delete(services, arguments, cancellation).await,
        "container.files.upload" => transfer(services, arguments, cancellation, true).await,
        "container.files.download" => transfer(services, arguments, cancellation, false).await,
        _ => failure(AppError::unsupported("未注册的容器工具")),
    }
}

/// 查询显式宿主上的容器列表，includeStopped 缺省为 false 以匹配运行态工作流。
async fn list(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerListArgs = parse_args(arguments)?;
        let query = ContainerListQuery {
            host_id: Some(host_id(&args.host_id)?),
            runtime: runtime(args.runtime.as_deref())?,
            include_stopped: args.include_stopped.unwrap_or(false),
        };
        let items = services
            .require_operations()?
            .containers
            .list(query, cancellation)
            .await?;
        let items = to_values(items)?;
        Ok(items)
    }
    .await;
    match result {
        Ok(items) => success(
            format!("查询到 {} 个容器", items.len()),
            json!({ "items": items }),
            items,
        ),
        Err(error) => failure(error),
    }
}

/// 检查单个容器并保留领域层已筛选的稳定数据投影。
async fn inspect(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerTargetArgs = parse_args(arguments)?;
        let target = target(&args.host_id, &args.container_id, args.runtime.as_deref())?;
        let item = services
            .require_operations()?
            .containers
            .inspect(target, cancellation)
            .await?;
        to_value(item)
    }
    .await;
    match result {
        Ok(item) => success("容器信息已读取".to_owned(), item.clone(), vec![item]),
        Err(error) => failure(error),
    }
}

/// 查询带时间戳的有界日志，并保留 stdout、stderr 和截断状态。
async fn logs(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerLogsArgs = parse_args(arguments)?;
        let logs = services
            .require_operations()?
            .containers
            .logs(
                target(&args.host_id, &args.container_id, args.runtime.as_deref())?,
                args.tail.unwrap_or(200),
                cancellation,
            )
            .await?;
        Ok(json!({
            "target": endpoint_target_value(&logs.target),
            "stdout": logs.stdout,
            "stderr": logs.stderr,
            "truncated": logs.truncated
        }))
    }
    .await;
    match result {
        Ok(data) => success("容器日志已读取".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 读取一次性 stats，不在 MCP 请求生命周期之外暗中启动轮询任务。
async fn stats(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerTargetArgs = parse_args(arguments)?;
        let item = services
            .require_operations()?
            .containers
            .stats(
                target(&args.host_id, &args.container_id, args.runtime.as_deref())?,
                cancellation,
            )
            .await?;
        to_value(item)
    }
    .await;
    match result {
        Ok(item) => success("容器资源快照已读取".to_owned(), item.clone(), vec![item]),
        Err(error) => failure(error),
    }
}

/// 容器写操作使用 application 的明确状态与回执；remove.force 缺席时不强删运行容器。
async fn mutate(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
    action: ContainerAction,
) -> CallToolResult {
    let result = async {
        let (target, force) = if matches!(&action, ContainerAction::Remove) {
            let args: ContainerRemoveArgs = parse_args(arguments)?;
            (
                target(&args.host_id, &args.container_id, args.runtime.as_deref())?,
                args.force.unwrap_or(false),
            )
        } else {
            let args: ContainerTargetArgs = parse_args(arguments)?;
            (
                target(&args.host_id, &args.container_id, args.runtime.as_deref())?,
                false,
            )
        };
        let receipt = services
            .require_operations()?
            .containers
            .mutate_with_force(target, action, force, cancellation)
            .await?;
        to_value(receipt)
    }
    .await;
    match result {
        Ok(receipt) => success("容器操作已完成".to_owned(), receipt.clone(), vec![receipt]),
        Err(error) => failure(error),
    }
}

/// 列出一层容器目录，保持 endpoint 的路径和元数据作为 UI/MCP 共享的查询结果。
async fn file_list(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerPathArgs = parse_args(arguments)?;
        let endpoint = endpoint(
            &args.host_id,
            &args.container_id,
            args.runtime.as_deref(),
            &args.path,
        )?;
        let listing = services
            .require_files()?
            .list_directory(cancellation, endpoint)
            .await?;
        let entries = listing
            .entries
            .into_iter()
            .map(file_entry_value)
            .collect::<Vec<_>>();
        let count = entries.len();
        Ok(json!({
            "path": endpoint_path(&listing.directory),
            "entries": entries,
            "count": count
        }))
    }
    .await;
    match result {
        Ok(data) => {
            let entities = data["entries"].as_array().cloned().unwrap_or_default();
            success("容器目录已读取".to_owned(), data, entities)
        }
        Err(error) => failure(error),
    }
}

/// 读取受限文件预览；二进制字节通过 base64 明确编码，不把任意数据伪装成文本。
async fn file_preview(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerPreviewArgs = parse_args(arguments)?;
        let max_bytes = args.max_bytes.unwrap_or(64 * 1024);
        if max_bytes == 0 {
            return Err(AppError::invalid("预览字节上限必须大于零"));
        }
        let preview = services
            .require_files()?
            .preview_file(
                cancellation,
                endpoint(
                    &args.host_id,
                    &args.container_id,
                    args.runtime.as_deref(),
                    &args.path,
                )?,
                max_bytes.min(MAX_FILE_PREVIEW_BYTES),
            )
            .await?;
        let content = if preview.binary {
            Value::Null
        } else {
            Value::String(String::from_utf8_lossy(&preview.bytes).into_owned())
        };
        Ok(json!({
            "path": endpoint_path(&preview.endpoint),
            "content": content,
            "contentBase64": BASE64.encode(&preview.bytes),
            "binary": preview.binary,
            "truncated": preview.truncated,
            "totalSize": preview.total_size,
            "revision": preview.revision.map(revision_value)
        }))
    }
    .await;
    match result {
        Ok(data) => success("容器文件预览已读取".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 原样保留 String 内容（空文本也有效），并优先保留 legacy revision 的 CAS 意图。
async fn file_write(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerWriteArgs = parse_args(arguments)?;
        match args.encoding.as_str() {
            "utf-8" | "utf-8-lossy" => {}
            _ => return Err(AppError::invalid("encoding 只支持 utf-8 或 utf-8-lossy")),
        }
        let endpoint = endpoint(
            &args.host_id,
            &args.container_id,
            args.runtime.as_deref(),
            &args.path,
        )?;
        let content = args.content;
        let strategy = write_strategy(
            args.expected_revision,
            args.create,
            args.overwrite_on_conflict,
        )?;
        let result = services
            .require_files()?
            .write_text_file(
                cancellation,
                WriteTextRequest {
                    endpoint,
                    content,
                    strategy,
                },
            )
            .await?;
        Ok(json!({
            "bytesWritten": result.bytes_written,
            "revision": revision_value(result.revision)
        }))
    }
    .await;
    match result {
        Ok(data) => success("容器文本已保存".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// Revision 始终优先形成 CAS；无 revision 时只按显式 flags 选择新建或覆盖。
fn write_strategy(
    revision: Option<RevisionArgs>,
    create: bool,
    overwrite: bool,
) -> Result<WriteTextStrategy, AppError> {
    if let Some(revision) = revision {
        return file_revision(revision).map(WriteTextStrategy::CompareRevision);
    }
    match (create, overwrite) {
        (true, false) => Ok(WriteTextStrategy::CreateNew),
        (false, true) | (true, true) => Ok(WriteTextStrategy::ExplicitOverwrite),
        (false, false) => Err(AppError::invalid(
            "无 expectedRevision 时必须显式请求新建或覆盖",
        )),
    }
}

/// 以显式单层 mkdir 用例创建目录，已存在或回执未知时保留 application 错误。
async fn file_create_directory(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerPathArgs = parse_args(arguments)?;
        services
            .require_files()?
            .create_directory(
                cancellation,
                endpoint(
                    &args.host_id,
                    &args.container_id,
                    args.runtime.as_deref(),
                    &args.path,
                )?,
            )
            .await?;
        Ok(json!({ "path": args.path }))
    }
    .await;
    match result {
        Ok(data) => success("容器目录已创建".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 只允许同一容器文件系统内 rename；跨目标请求交由 FileService 明确拒绝。
async fn file_rename(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerRenameArgs = parse_args(arguments)?;
        let source = endpoint(
            &args.host_id,
            &args.container_id,
            args.runtime.as_deref(),
            &args.from_path,
        )?;
        let destination = endpoint(
            &args.host_id,
            &args.container_id,
            args.runtime.as_deref(),
            &args.to_path,
        )?;
        services
            .require_files()?
            .rename(cancellation, source, destination)
            .await?;
        Ok(json!({ "fromPath": args.from_path, "toPath": args.to_path }))
    }
    .await;
    match result {
        Ok(data) => success("容器路径已重命名".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 将八进制权限文本转成受限整数，禁止把任意字符传给容器命令。
async fn file_chmod(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerChmodArgs = parse_args(arguments)?;
        let text = args.mode.trim();
        let digits = text.strip_prefix("0o").unwrap_or(text);
        let mode = u32::from_str_radix(digits, 8)
            .map_err(|_| AppError::invalid("mode 必须是八进制权限"))?;
        services
            .require_files()?
            .chmod(
                cancellation,
                endpoint(
                    &args.host_id,
                    &args.container_id,
                    args.runtime.as_deref(),
                    &args.path,
                )?,
                mode,
            )
            .await?;
        Ok(json!({ "path": args.path, "mode": format!("{mode:04o}") }))
    }
    .await;
    match result {
        Ok(data) => success("容器文件权限已修改".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 文件删除始终明确区分单文件与容器内递归目录删除。
async fn file_delete(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: ContainerDeleteArgs = parse_args(arguments)?;
        let directory = args.directory;
        services
            .require_files()?
            .delete(
                cancellation,
                endpoint(
                    &args.host_id,
                    &args.container_id,
                    args.runtime.as_deref(),
                    &args.path,
                )?,
                if directory {
                    DeleteTargetKind::Directory
                } else {
                    DeleteTargetKind::File
                },
            )
            .await?;
        Ok(json!({ "path": args.path, "directory": directory, "deleted": true }))
    }
    .await;
    match result {
        Ok(data) => success("容器路径已删除".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 上传和下载只入统一 transfer queue；响应明确表示受理，不伪称文件已经提交。
async fn transfer(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
    upload: bool,
) -> CallToolResult {
    let result = async {
        let args: ContainerTransferArgs = parse_args(arguments)?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("容器传输请求已取消"));
        }
        let local = FileEndpoint::Local {
            path: PathBuf::from(non_empty(&args.local_path, "localPath")?),
        };
        let remote = endpoint(
            &args.host_id,
            &args.container_id,
            args.runtime.as_deref(),
            &args.remote_path,
        )?;
        let kind = match args.kind.as_str() {
            "file" => TransferKind::File,
            "directory" => TransferKind::Directory,
            _ => return Err(AppError::invalid("kind 只支持 file 或 directory")),
        };
        let request = TransferRequest {
            source: if upload {
                local.clone()
            } else {
                remote.clone()
            },
            destination: if upload {
                remote.clone()
            } else {
                local.clone()
            },
            kind,
            // 旧 docker cp 对目的内容执行覆盖；队列仍以受校验的同目录提交保护已有目标。
            conflict_policy: ConflictPolicy::Overwrite,
            // 冻结的容器传输 schema 没有 idle timeout 字段，沿用队列默认预算。
            idle_timeout_seconds: None,
        };
        let summary = services.require_transfers()?.enqueue(request).await?;
        Ok(json!({
            "transferId": summary.id.as_str(),
            "status": transfer_status(summary.status),
            "accepted": true,
            "completed": false,
            "kind": if kind == TransferKind::File { "file" } else { "directory" },
            "source": endpoint_value(&summary.request.source),
            "destination": endpoint_value(&summary.request.destination),
            "bytesTransferred": summary.bytes_transferred,
            "totalBytes": summary.total_bytes
        }))
    }
    .await;
    match result {
        Ok(data) => success(
            if upload {
                "容器上传任务已受理，任务仍在队列中".to_owned()
            } else {
                "容器下载任务已受理，任务仍在队列中".to_owned()
            },
            data.clone(),
            vec![data],
        ),
        Err(error) => failure(error),
    }
}

/// 将无凭据的 MCP 字段转为容器根内的规范化 endpoint；不复用 SFTP 相对路径语义。
fn endpoint(
    host_id_value: &str,
    container_id: &str,
    runtime_value: Option<&str>,
    path: &str,
) -> Result<FileEndpoint, AppError> {
    let path = RemotePath::parse_container(non_empty(path, "path")?)
        .map_err(|_| AppError::invalid("容器文件路径无效或越过容器根"))?;
    Ok(FileEndpoint::Container {
        host_id: Some(host_id(host_id_value)?),
        runtime: runtime(runtime_value)?,
        container_id: non_empty(container_id, "containerId")?.to_owned(),
        path,
    })
}

/// 校验 wire runtime，并保持可选字段缺省为现行 Docker 默认值。
fn runtime(runtime_value: Option<&str>) -> Result<ContainerRuntime, AppError> {
    match runtime_value.unwrap_or("docker") {
        "docker" => Ok(ContainerRuntime::Docker),
        "podman" => Ok(ContainerRuntime::Podman),
        _ => Err(AppError::invalid("runtime 只支持 docker 或 podman")),
    }
}

/// 将主机标识限制为保存配置中可解析的 opaque ID，不接收地址或凭据。
fn host_id(value: &str) -> Result<HostId, AppError> {
    HostId::parse(non_empty(value, "hostId")?)
        .map_err(|_| AppError::invalid("container hostId 无效"))
}

/// 容器运行时目标校验集中在 MCP 边界，业务服务仍负责目标状态和实际命令规则。
fn target(
    host_id_value: &str,
    container_id: &str,
    runtime_value: Option<&str>,
) -> Result<ContainerTarget, AppError> {
    Ok(ContainerTarget {
        host_id: Some(host_id(host_id_value)?),
        runtime: runtime(runtime_value)?,
        container_id: non_empty(container_id, "containerId")?.to_owned(),
    })
}

/// 校验业务标识和路径非空；普通 String 的字段存在性由精确 DTO 保证，内容允许为空。
fn non_empty<'a>(value: &'a str, field: &'static str) -> Result<&'a str, AppError> {
    (!value.trim().is_empty())
        .then_some(value)
        .ok_or_else(|| AppError::invalid(field))
}

/// 把兼容 revision 字段映射到领域快照，用实际 mode 和摘要参与并发比较。
fn file_revision(args: RevisionArgs) -> Result<FileRevision, AppError> {
    let permissions = match (args.permissions_mode, args.permissions) {
        (Some(mode), _) => Some(mode),
        (None, Some(value)) => {
            let digits = value.trim().trim_start_matches("0o");
            Some(
                u32::from_str_radix(digits, 8)
                    .map_err(|_| AppError::invalid("revision permissions 无效"))?,
            )
        }
        (None, None) => None,
    };
    Ok(FileRevision {
        size: args.size,
        modified: args.modified,
        permissions,
        content_digest: args.content_sha256,
    })
}

/// 输出 revision 使用现行 MCP camelCase 名称，不暴露内部 Rust 字段命名。
fn revision_value(revision: FileRevision) -> Value {
    json!({
        "size": revision.size,
        "modified": revision.modified,
        "permissionsMode": revision.permissions,
        "contentSha256": revision.content_digest
    })
}

/// 构造目录条目 JSON，同时保留 kind 和便于旧客户端读取的 isDirectory。
fn file_entry_value(entry: kerminal_domain::files::FileEntry) -> Value {
    let kind = match entry.kind {
        kerminal_domain::files::FileEntryKind::File => "file",
        kerminal_domain::files::FileEntryKind::Directory => "directory",
        kerminal_domain::files::FileEntryKind::Symlink => "symlink",
        kerminal_domain::files::FileEntryKind::Other => "other",
    };
    json!({
        "name": entry.name,
        "path": endpoint_path(&entry.endpoint),
        "kind": kind,
        "isDirectory": kind == "directory",
        "size": entry.size,
        "modified": entry.modified,
        "permissionsMode": entry.permissions,
        "raw": entry.raw
    })
}

/// 提取 POSIX 远端路径用于结果展示；endpoint 类型在服务层已受限为容器目标。
fn endpoint_path(endpoint: &FileEndpoint) -> Option<&str> {
    endpoint.remote_path().map(RemotePath::as_str)
}

/// 以 host/runtime/container/path 唯一标识端点，不返回凭据或连接状态。
fn endpoint_value(endpoint: &FileEndpoint) -> Value {
    match endpoint {
        FileEndpoint::Local { path } => json!({ "kind": "local", "path": path }),
        FileEndpoint::Remote { host_id, path } => json!({
            "kind": "remote", "hostId": host_id.as_str(), "path": path.as_str()
        }),
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => json!({
            "kind": "container",
            "hostId": host_id.as_ref().map(HostId::as_str),
            "runtime": if *runtime == ContainerRuntime::Docker { "docker" } else { "podman" },
            "containerId": container_id,
            "path": path.as_str()
        }),
    }
}

/// 描述日志目标时只返回应用级资源标识。
fn endpoint_target_value(target: &ContainerTarget) -> Value {
    json!({
        "hostId": target.host_id.as_ref().map(HostId::as_str),
        "runtime": if target.runtime == ContainerRuntime::Docker { "docker" } else { "podman" },
        "containerId": target.container_id
    })
}

/// 转换队列状态为固定小写 wire 枚举，避免依赖 Rust Debug 输出。
fn transfer_status(status: kerminal_domain::files::TransferStatus) -> &'static str {
    use kerminal_domain::files::TransferStatus;
    match status {
        TransferStatus::Queued => "queued",
        TransferStatus::Connecting => "connecting",
        TransferStatus::Transferring => "transferring",
        TransferStatus::WaitingForRecovery => "waitingForRecovery",
        TransferStatus::Committing => "committing",
        TransferStatus::Cancelling => "cancelling",
        TransferStatus::Succeeded => "succeeded",
        TransferStatus::Unchanged => "succeeded",
        TransferStatus::Skipped => "skipped",
        TransferStatus::Failed => "failed",
        TransferStatus::Cancelled => "cancelled",
    }
}

/// 将领域结果序列化失败保持为内部错误，不静默构造缺字段成功响应。
fn to_value(value: impl serde::Serialize) -> Result<Value, AppError> {
    serde_json::to_value(value).map_err(|_| AppError::infrastructure("容器结果无法编码"))
}

/// 将列表投影转换失败统一处理，避免逐项 encode 失败后返回部分数据。
fn to_values<T: serde::Serialize>(items: Vec<T>) -> Result<Vec<Value>, AppError> {
    items.into_iter().map(to_value).collect()
}

#[cfg(test)]
#[path = "../../tests/unit/container_file_write_strategy.rs"]
mod write_strategy_tests;

#[cfg(test)]
#[path = "../../tests/unit/container_file_endpoint.rs"]
mod file_endpoint_tests;

#[cfg(test)]
#[path = "../../tests/unit/container_file_entry_projection.rs"]
mod file_entry_projection_tests;
