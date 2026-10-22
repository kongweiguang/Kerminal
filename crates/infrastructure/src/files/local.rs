//! 本机文件系统适配器；目录删除不跟随符号链接，编辑通过临时文件原子提交。
//!
//! @author kongweiguang

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};
use async_trait::async_trait;
use kerminal_application::{AppError, AppResult, files::FileAccessProvider};
use kerminal_domain::files::{
    DeleteTargetKind, DirectoryListing, FileEndpoint, FileEntry, FileEntryKind, FilePreview,
    FileRevision, LineEnding, TextDocument, WriteTextRequest, WriteTextResult, WriteTextStrategy,
};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 真实本机目录访问端口；路径来自用户选择，不从主机配置中拼接秘密。
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFileProvider;

#[async_trait]
impl FileAccessProvider for LocalFileProvider {
    /// 本机适配器只接受 Local，避免把远程路径误交给宿主操作系统。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        matches!(endpoint, FileEndpoint::Local { .. })
    }

    /// 目录枚举只读且可能较慢，取消时丢弃未完成查询并释放迭代器，不保留后台工作。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        cancellable_read(&cancellation, async {
            let directory = local_endpoint(&endpoint)?;
            let metadata = fs::metadata(directory).await.map_err(map_io_error)?;
            if !metadata.is_dir() {
                return Err(AppError::conflict("所选路径不是目录"));
            }
            let mut reader = fs::read_dir(directory).await.map_err(map_io_error)?;
            let mut entries = Vec::new();
            while let Some(entry) = reader.next_entry().await.map_err(map_io_error)? {
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).await.map_err(map_io_error)?;
                let kind = local_entry_kind(&metadata);
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_hidden = local_entry_is_hidden(&name, &metadata);
                entries.push(FileEntry {
                    endpoint: FileEndpoint::Local { path },
                    name,
                    kind,
                    size: metadata.is_file().then_some(metadata.len()),
                    modified: metadata.modified().ok().map(format_time),
                    permissions: local_permissions(&metadata),
                    is_hidden,
                    raw: None,
                });
            }
            entries.sort_by(|left, right| {
                let left_directory = left.kind == FileEntryKind::Directory;
                let right_directory = right.kind == FileEntryKind::Directory;
                right_directory
                    .cmp(&left_directory)
                    .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            });
            Ok(DirectoryListing {
                directory: endpoint,
                entries,
            })
        })
        .await
    }

    /// 预览以流式有界读取；用户取消后停止读文件，避免外部工具拖住大文件查询。
    async fn preview_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<FilePreview> {
        cancellable_read(&cancellation, async {
            let path = local_endpoint(&endpoint)?;
            let metadata = fs::metadata(path).await.map_err(map_io_error)?;
            if !metadata.is_file() {
                return Err(AppError::conflict("预览目标不是普通文件"));
            }
            let (bytes, truncated) = read_prefix(path, max_bytes, &cancellation).await?;
            let binary = is_binary_preview(&bytes, truncated);
            Ok(FilePreview {
                endpoint,
                bytes,
                total_size: Some(metadata.len()),
                truncated: truncated || metadata.len() > max_bytes as u64,
                binary,
                revision: Some(local_revision(&metadata, None)),
            })
        })
        .await
    }

    /// 文本读取可取消并仍受大小上限约束；截断内容标为只读以免保存时丢掉尾部。
    async fn read_text_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<TextDocument> {
        cancellable_read(&cancellation, async {
            let path = local_endpoint(&endpoint)?;
            let metadata = fs::symlink_metadata(path).await.map_err(map_io_error)?;
            if metadata.file_type().is_symlink() {
                return Err(AppError::unsupported(
                    "符号链接可预览，但文本编辑需打开其真实文件路径",
                ));
            }
            if !metadata.is_file() {
                return Err(AppError::conflict("编辑目标不是普通文件"));
            }
            let (bytes, truncated) = read_prefix(path, max_bytes, &cancellation).await?;
            let content = String::from_utf8(bytes.clone())
                .map_err(|_| AppError::unsupported("文件不是 UTF-8 文本，不能作为文本编辑"))?;
            let line_ending = line_ending(&bytes);
            Ok(TextDocument {
                endpoint,
                content,
                line_ending,
                revision: local_revision(&metadata, Some(&bytes)),
                readonly: truncated || metadata.permissions().readonly(),
                truncated,
            })
        })
        .await
    }

    /// 按明确策略验证 revision；staging 可取消，提交开始后必须等待原子提交的确定结果。
    async fn write_text_file(
        &self,
        cancellation: CancellationToken,
        request: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        let path = local_endpoint(&request.endpoint)?;
        ensure_not_cancelled(&cancellation)?;
        let current = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("文件写入已取消")),
            result = fs::symlink_metadata(path) => result,
        };
        let expected_revision = match (&request.strategy, current) {
            (WriteTextStrategy::CreateNew, Ok(_)) => {
                return Err(AppError::conflict("新建目标已经存在"));
            }
            (WriteTextStrategy::CreateNew, Err(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                None
            }
            (WriteTextStrategy::CreateNew, Err(error)) => return Err(map_io_error(error)),
            (WriteTextStrategy::CompareRevision(expected), Ok(metadata)) => {
                if metadata.file_type().is_symlink() {
                    return Err(AppError::unsupported(
                        "符号链接可预览，但文本编辑需打开其真实文件路径",
                    ));
                }
                if !metadata.is_file() {
                    return Err(AppError::conflict("编辑目标不是普通文件"));
                }
                let digest = if expected.content_digest.is_some() {
                    Some(read_full_for_revision(path, expected.size, &cancellation).await?)
                } else {
                    None
                };
                if local_revision(&metadata, digest.as_deref()) != *expected {
                    return Err(AppError::conflict("文件在编辑期间已被其它操作修改"));
                }
                Some(expected.clone())
            }
            (WriteTextStrategy::CompareRevision(_), Err(error)) => return Err(map_io_error(error)),
            (WriteTextStrategy::ExplicitOverwrite, Ok(metadata)) => {
                if metadata.file_type().is_symlink() {
                    return Err(AppError::unsupported(
                        "符号链接可预览，但文本编辑需打开其真实文件路径",
                    ));
                }
                if !metadata.is_file() {
                    return Err(AppError::conflict("覆盖目标不是普通文件"));
                }
                None
            }
            (WriteTextStrategy::ExplicitOverwrite, Err(error)) => return Err(map_io_error(error)),
        };

        let temporary = temporary_sibling(path, "edit");
        ensure_not_cancelled(&cancellation)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(map_io_error)?;
        let write_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
            result = file.write_all(request.content.as_bytes()) => result,
        };
        if let Err(error) = write_result {
            drop(file);
            cleanup_temporary(&temporary).await;
            return Err(if cancellation.is_cancelled() {
                AppError::cancelled("文件写入已取消")
            } else {
                map_io_error(error)
            });
        }
        let sync_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
            result = file.sync_all() => result,
        };
        if let Err(error) = sync_result {
            drop(file);
            cleanup_temporary(&temporary).await;
            return Err(if cancellation.is_cancelled() {
                AppError::cancelled("文件写入已取消")
            } else {
                map_io_error(error)
            });
        }
        drop(file);
        match request.strategy {
            WriteTextStrategy::CreateNew => {
                ensure_before_commit(&cancellation, &temporary).await?;
                if let Err(error) = commit_file(&temporary, path, CommitMode::CreateNew).await {
                    cleanup_temporary(&temporary).await;
                    return Err(error);
                }
            }
            WriteTextStrategy::CompareRevision(_) => {
                let expected = expected_revision
                    .as_ref()
                    .ok_or_else(|| AppError::invalid("更新文件必须提供打开时 revision"))?;
                if let Err(error) = verify_expected_revision(path, expected, &cancellation).await {
                    cleanup_temporary(&temporary).await;
                    return Err(error);
                }
                ensure_before_commit(&cancellation, &temporary).await?;
                if let Err(error) = commit_file(&temporary, path, CommitMode::ReplaceExisting).await
                {
                    cleanup_temporary(&temporary).await;
                    return Err(error);
                }
            }
            WriteTextStrategy::ExplicitOverwrite => {
                if let Err(error) = verify_regular_edit_target(path).await {
                    cleanup_temporary(&temporary).await;
                    return Err(error);
                }
                ensure_before_commit(&cancellation, &temporary).await?;
                if let Err(error) = commit_file(&temporary, path, CommitMode::ReplaceExisting).await
                {
                    cleanup_temporary(&temporary).await;
                    return Err(error);
                }
            }
        }
        let metadata = fs::metadata(path).await.map_err(|error| {
            AppError::outcome_unknown_with_source("本机文件已提交但 revision 回读失败", error)
        })?;
        let content = request.content.as_bytes();
        Ok(WriteTextResult {
            bytes_written: content.len() as u64,
            revision: local_revision(&metadata, Some(content)),
        })
    }

    /// 创建单层目录；已存在或父路径不可写由文件系统错误映射为安全结果。
    async fn create_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        let path = local_endpoint(&endpoint)?;
        ensure_not_cancelled(&cancellation)?;
        fs::create_dir(path).await.map_err(map_io_error)
    }

    /// 本机 no-clobber rename 使用共享存储提交原语，避免各适配器重复维护平台 FFI。
    async fn rename(
        &self,
        cancellation: CancellationToken,
        source: FileEndpoint,
        destination: FileEndpoint,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        let source_path = local_endpoint(&source)?;
        let destination_path = local_endpoint(&destination)?;
        ensure_not_cancelled(&cancellation)?;
        commit_file(source_path, destination_path, CommitMode::MoveNoReplace).await
    }

    /// Unix 权限位可精确设置；其它平台显式报告不支持，不把只读位伪装成 chmod。
    async fn chmod(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        mode: u32,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(local_endpoint(&endpoint)?)
                .await
                .map_err(map_io_error)?
                .permissions();
            permissions.set_mode(mode);
            ensure_not_cancelled(&cancellation)?;
            fs::set_permissions(local_endpoint(&endpoint)?, permissions)
                .await
                .map_err(map_io_error)
        }
        #[cfg(not(unix))]
        {
            let _ = (endpoint, mode);
            Err(AppError::unsupported("当前平台不支持 Unix chmod 权限位"))
        }
    }

    /// 本机目录按文件类型明确删除；符号链接不会被当成子目录跟随。
    async fn delete(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        kind: DeleteTargetKind,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        let path = local_endpoint(&endpoint)?;
        let metadata = fs::symlink_metadata(path).await.map_err(map_io_error)?;
        ensure_not_cancelled(&cancellation)?;
        match (kind, local_entry_kind(&metadata)) {
            (DeleteTargetKind::File, FileEntryKind::Directory) => {
                return Err(AppError::conflict("目标是目录，不能按文件删除"));
            }
            (DeleteTargetKind::Directory, FileEntryKind::Directory) => {
                remove_directory_tree(path, &cancellation).await
            }
            (DeleteTargetKind::Directory, _) => Err(AppError::conflict("目标不是目录")),
            (DeleteTargetKind::File, _) => fs::remove_file(path).await.map_err(map_io_error),
        }
    }
}

/// 将同步的 OS 提交原语移出 Tokio worker，并把“未提交”和“结果未知”映射为安全应用错误。
///
/// @author kongweiguang
async fn commit_file(source: &Path, destination: &Path, mode: CommitMode) -> AppResult<()> {
    let source = absolute_path(source).map_err(map_io_error)?;
    let destination = absolute_path(destination).map_err(map_io_error)?;
    tokio::task::spawn_blocking(move || commit_staged_file(&source, &destination, mode))
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("本机文件提交任务结果未知，请重新读取确认", error)
        })?
        .map_err(map_atomic_error)
}

/// 解析相对本机路径时保留用户选择的链接和末端路径，不做 canonicalize 跟随重定向。
///
/// @author kongweiguang
fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// 原子提交在目录同步失败时可能已完成 rename，保留可区分的未知结果文案供 UI 回读。
///
/// @author kongweiguang
fn map_atomic_error(error: AtomicCommitError) -> AppError {
    match error {
        AtomicCommitError::NotCommitted(source) => {
            AppError::infrastructure_with_source("本机文件尚未提交", source)
        }
        AtomicCommitError::CommitUnknown(source) => {
            AppError::outcome_unknown_with_source("本机文件提交结果未知，请重新读取确认", source)
        }
    }
}

/// 从本机 endpoint 提取 PathBuf；显式拒绝跨协议 endpoint 避免宿主路径解释远端路径。
fn local_endpoint(endpoint: &FileEndpoint) -> AppResult<&Path> {
    match endpoint {
        FileEndpoint::Local { path } if !path.as_os_str().is_empty() => Ok(path),
        FileEndpoint::Local { .. } => Err(AppError::invalid("本机文件路径不能为空")),
        _ => Err(AppError::unsupported("本机文件适配器不处理远程目标")),
    }
}

/// 对 CompareRevision 写入在提交前重读 revision；通用文件系统没有跨进程 CAS 原语，因此仍保留极短外部写入竞态。
///
/// @author kongweiguang
async fn verify_expected_revision(
    path: &Path,
    expected: &FileRevision,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    ensure_not_cancelled(cancellation)?;
    verify_regular_edit_target(path).await?;
    let metadata = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(AppError::cancelled("文件写入已取消")),
        result = fs::symlink_metadata(path) => result.map_err(map_io_error)?,
    };
    let digest = if expected.content_digest.is_some() {
        Some(read_full_for_revision(path, expected.size, cancellation).await?)
    } else {
        None
    };
    if local_revision(&metadata, digest.as_deref()) != *expected {
        return Err(AppError::conflict("文件在保存提交前已被其它操作修改"));
    }
    Ok(())
}

/// 覆盖操作仍拒绝链接和非普通文件，避免用户确认覆盖后重定向到树外目标。
///
/// @author kongweiguang
async fn verify_regular_edit_target(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => Ok(()),
        Ok(_) => Err(AppError::conflict("编辑目标类型在保存期间发生变化")),
        Err(error) => Err(map_io_error(error)),
    }
}

/// 读取受限前缀并额外探测一个字节，以区分刚好达到限制和真实截断。
async fn read_prefix(
    path: &Path,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> AppResult<(Vec<u8>, bool)> {
    cancellable_read(cancellation, async {
        let file = File::open(path).await.map_err(map_io_error)?;
        let mut limited = file.take(max_bytes.saturating_add(1) as u64);
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
        limited
            .read_to_end(&mut bytes)
            .await
            .map_err(map_io_error)?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        Ok((bytes, truncated))
    })
    .await
}

/// 为远程编辑 revision 按需读取完整的已打开文件，且只有文件大小仍一致时才计算摘要。
async fn read_full_for_revision(
    path: &Path,
    expected_size: u64,
    cancellation: &CancellationToken,
) -> AppResult<Vec<u8>> {
    if expected_size > kerminal_application::files::MAX_TEXT_FILE_READ_BYTES as u64 {
        return Err(AppError::unsupported(
            "大于编辑上限的文件不能做内容级 revision 比对",
        ));
    }
    cancellable_read(cancellation, async {
        let file = File::open(path).await.map_err(map_io_error)?;
        let mut limited = file.take(expected_size.saturating_add(1));
        let mut bytes = Vec::with_capacity(expected_size as usize);
        limited
            .read_to_end(&mut bytes)
            .await
            .map_err(map_io_error)?;
        if bytes.len() as u64 != expected_size {
            return Err(AppError::conflict("文件在 revision 校验期间已变化"));
        }
        Ok(bytes)
    })
    .await
}

/// 文件查询可撤销并释放正在等待的 OS 操作，不把取消用于会改变用户数据的提交。
///
/// @author kongweiguang
async fn cancellable_read<T>(
    cancellation: &CancellationToken,
    operation: impl Future<Output = AppResult<T>>,
) -> AppResult<T> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AppError::cancelled("文件读取已取消")),
        result = operation => result,
    }
}

/// 在移交原子提交 helper 前清理 staging；提交调用开始后必须等待其确定结果。
///
/// @author kongweiguang
async fn ensure_before_commit(cancellation: &CancellationToken, temporary: &Path) -> AppResult<()> {
    if cancellation.is_cancelled() {
        cleanup_temporary(temporary).await;
        Err(AppError::cancelled("文件写入已取消"))
    } else {
        Ok(())
    }
}

/// 递归删除前拒绝后续步骤，尽量限制取消时已经删除的目录范围。
///
/// @author kongweiguang
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("文件操作已取消"))
    } else {
        Ok(())
    }
}

/// 生成元数据 revision；编辑器读取时附带 SHA-256，列表和预览不做昂贵的全文件散列。
fn local_revision(metadata: &std::fs::Metadata, content: Option<&[u8]>) -> FileRevision {
    let modified = metadata.modified().ok().map(format_time);
    FileRevision {
        size: metadata.len(),
        modified,
        permissions: local_permissions(metadata),
        content_digest: content.map(sha256_hex),
    }
}

/// 转换本机元数据到 UI 的精简类型，Windows 只公开可写性而不猜测 Unix 模式。
fn local_permissions(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        Some(if metadata.permissions().readonly() {
            0o444
        } else {
            0o666
        })
    }
}

/// 把系统时间统一序列化为 Unix 纳秒文本，减少不同平台时间精度差异导致的 revision 混淆。
fn format_time(value: SystemTime) -> String {
    value
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

/// 根据元数据文件类型分类，符号链接单独显示且不会被标成目录。
fn local_entry_kind(metadata: &std::fs::Metadata) -> FileEntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        FileEntryKind::Symlink
    } else if file_type.is_dir() {
        FileEntryKind::Directory
    } else if file_type.is_file() {
        FileEntryKind::File
    } else {
        FileEntryKind::Other
    }
}

/// 按平台真实隐藏语义投影本机目录项；Windows 属性与点文件规则并存，不跟随符号链接。
///
/// @author kongweiguang
#[cfg(windows)]
fn local_entry_is_hidden(name: &str, metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;

    is_dot_hidden_name(name) || metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

/// Unix 本机隐藏项遵循约定的前导点语义，metadata 参数仅用于和 Windows 分支保持调用一致。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn local_entry_is_hidden(name: &str, _metadata: &std::fs::Metadata) -> bool {
    is_dot_hidden_name(name)
}

/// 排除目录遍历标记，只把真实名称以点开头的文件或目录视为隐藏项。
///
/// @author kongweiguang
fn is_dot_hidden_name(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

/// 识别单一或混合行尾，保存由编辑器沿用原文件风格。
fn line_ending(bytes: &[u8]) -> LineEnding {
    let crlf = bytes.windows(2).filter(|pair| *pair == b"\r\n").count();
    let lf = bytes
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        .saturating_sub(crlf);
    match (crlf > 0, lf > 0) {
        (true, true) => LineEnding::Mixed,
        (true, false) => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}

/// 临时文件和正式目标同目录，保证后续 rename 不跨文件系统并保持原子性。
fn temporary_sibling(path: &Path, purpose: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    path.with_file_name(format!(
        ".{file_name}.kerminal-{purpose}-{}.partial",
        Uuid::new_v4()
    ))
}

/// 递归移除真实目录时逐层重新检查链接身份，防止递归遍历跟随目录符号链接。
fn remove_directory_tree<'a>(
    path: &'a Path,
    cancellation: &'a CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<()>> + Send + 'a>> {
    Box::pin(async move {
        ensure_not_cancelled(cancellation)?;
        let metadata = fs::symlink_metadata(path).await.map_err(map_io_error)?;
        if local_entry_kind(&metadata) != FileEntryKind::Directory {
            return Err(AppError::conflict("递归删除期间目标目录类型已变化"));
        }
        let mut entries = fs::read_dir(path).await.map_err(map_io_error)?;
        while let Some(entry) = entries.next_entry().await.map_err(map_io_error)? {
            ensure_not_cancelled(cancellation)?;
            let child = entry.path();
            let child_metadata = fs::symlink_metadata(&child).await.map_err(map_io_error)?;
            if local_entry_kind(&child_metadata) == FileEntryKind::Directory {
                remove_directory_tree(&child, cancellation).await?;
            } else {
                ensure_not_cancelled(cancellation)?;
                fs::remove_file(&child).await.map_err(map_io_error)?;
            }
        }
        ensure_not_cancelled(cancellation)?;
        fs::remove_dir(path).await.map_err(map_io_error)
    })
}

/// SHA-256 摘要仅用于 revision/checkpoint 验证，不暴露文件内容或凭据信息。
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 将系统错误映射成稳定分类并保留 source 供内部诊断，公开 Display 不展开原文。
fn map_io_error(error: io::Error) -> AppError {
    match error.kind() {
        io::ErrorKind::NotFound => AppError::not_found("本机文件或目录不存在"),
        io::ErrorKind::AlreadyExists => AppError::conflict("本机目标已经存在"),
        io::ErrorKind::PermissionDenied => AppError::conflict("本机文件权限不足"),
        _ => AppError::infrastructure_with_source("本机文件操作失败", error),
    }
}

/// 删除失败的临时写入文件并只记录错误类别，避免日志暴露用户路径或覆盖原始失败原因。
///
/// @author kongweiguang
async fn cleanup_temporary(path: &Path) {
    if let Err(error) = fs::remove_file(path).await
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(error_kind = ?error.kind(), "本机临时文件清理失败");
    }
}

/// 只允许截断边界上的不完整 UTF-8 尾序列作为文本预览，防止中文被误判为二进制。
///
/// @author kongweiguang
fn is_binary_preview(bytes: &[u8], truncated: bool) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => false,
        Err(error) => !truncated || error.error_len().is_some(),
    }
}

#[cfg(test)]
#[path = "../../tests/files/local_preview.rs"]
mod preview_tests;
