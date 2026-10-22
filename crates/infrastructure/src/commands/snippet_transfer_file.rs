//! 用户片段传输文件的有界读取与原子导出实现。
//!
//! @author kongweiguang

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{
        SnippetTransferFileConfirmation, SnippetTransferFilePort, SnippetTransferFileWriteReceipt,
    },
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

/// 片段 JSON 文件不会保存在配置根中，路径只来自用户明确选择的系统文件对话框。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSnippetTransferFileAdapter;

#[async_trait]
impl SnippetTransferFilePort for SystemSnippetTransferFileAdapter {
    /// 将磁盘读取放入 Tokio 阻塞池，并在每次分块读取时响应协作取消。
    async fn read_import_file(
        &self,
        path: PathBuf,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<String> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("片段导入文件读取已取消"));
        }
        tokio::task::spawn_blocking(move || {
            read_import_file_blocking(&path, max_bytes, &cancellation)
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("片段导入文件读取任务意外终止", source)
        })?
    }

    /// 在目标同目录写入 staging 并原子替换；提交未知或 worker 失去回执时不当作可重试失败。
    ///
    /// @author kongweiguang
    async fn write_export_file(
        &self,
        path: PathBuf,
        source: String,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<SnippetTransferFileWriteReceipt> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("片段导出文件写入已取消"));
        }
        tokio::task::spawn_blocking(move || {
            write_export_file_blocking(&path, source.as_bytes(), max_bytes, &cancellation)
        })
        .await
        .map_err(export_worker_join_error)?
    }
}

/// 阻塞写入 worker 可能在原子替换后丢失回执，因此把 JoinError 保留为未知提交。
///
/// @author kongweiguang
fn export_worker_join_error(source: tokio::task::JoinError) -> AppError {
    AppError::outcome_unknown_with_source("片段导出结果未知，请检查所选文件后再决定下一步", source)
}

/// 只接受普通 UTF-8 文件，并通过分块上限和取消检查防止读取任意大内容。
fn read_import_file_blocking(
    path: &Path,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> AppResult<String> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("片段导入文件读取已取消"));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| AppError::infrastructure_with_source("无法读取所选片段文件", source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::invalid("片段导入目标必须是普通文件"));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(AppError::invalid("片段导入文件超过 32 MiB 限制"));
    }

    let mut file = File::open(path)
        .map_err(|source| AppError::infrastructure_with_source("无法打开所选片段文件", source))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut chunk = [0_u8; 8192];
    loop {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("片段导入文件读取已取消"));
        }
        let count = file.read(&mut chunk).map_err(|source| {
            AppError::infrastructure_with_source("读取所选片段文件失败", source)
        })?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > max_bytes {
            return Err(AppError::invalid("片段导入文件超过 32 MiB 限制"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }

    String::from_utf8(bytes).map_err(|_| AppError::invalid("片段导入文件不是有效 UTF-8 文本"))
}

/// 将选中路径规范为现有父目录中的文件项，并选择不覆盖或原子替换提交策略。
fn export_destination(path: &Path) -> AppResult<(PathBuf, CommitMode)> {
    if path.as_os_str().is_empty() {
        return Err(AppError::invalid("片段导出文件路径无效"));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| AppError::infrastructure_with_source("无法确定片段导出位置", source))?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::invalid("片段导出文件路径无效"))?;
    let parent = absolute
        .parent()
        .ok_or_else(|| AppError::invalid("片段导出位置必须是已有目录"))?;
    let parent = fs::canonicalize(parent)
        .map_err(|source| AppError::infrastructure_with_source("无法访问片段导出位置", source))?;
    if !fs::metadata(&parent)
        .map_err(|source| AppError::infrastructure_with_source("无法检查片段导出位置", source))?
        .is_dir()
    {
        return Err(AppError::invalid("片段导出位置必须是目录"));
    }

    let destination = parent.join(file_name);
    let mode = match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(AppError::invalid("片段导出目标必须是普通文件"));
        }
        Ok(_) => CommitMode::ReplaceExisting,
        Err(error) if error.kind() == io::ErrorKind::NotFound => CommitMode::CreateNew,
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "无法检查片段导出目标",
                source,
            ));
        }
    };
    Ok((destination, mode))
}

/// staging 与目标位于同一目录，确保提交可原子完成且取消不会留下半份 JSON。
///
/// @author kongweiguang
fn write_export_file_blocking(
    path: &Path,
    source: &[u8],
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> AppResult<SnippetTransferFileWriteReceipt> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("片段导出文件写入已取消"));
    }
    if source.len() > max_bytes {
        return Err(AppError::invalid("片段导出文件超过 32 MiB 限制"));
    }

    let (destination, mode) = export_destination(path)?;
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::invalid("片段导出位置无效"))?;
    let (staging_path, mut file) = create_staging_file(parent)?;
    if let Err(source) = file.write_all(source).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staging_path);
        return Err(AppError::infrastructure_with_source(
            "写入片段导出暂存文件失败",
            source,
        ));
    }
    drop(file);

    if cancellation.is_cancelled() {
        let _ = fs::remove_file(&staging_path);
        return Err(AppError::cancelled("片段导出文件写入已取消"));
    }

    match commit_staged_file(&staging_path, &destination, mode) {
        Ok(()) => Ok(SnippetTransferFileWriteReceipt {
            bytes_written: source.len(),
            confirmation: SnippetTransferFileConfirmation::AtomicCommit,
        }),
        Err(AtomicCommitError::NotCommitted(error)) => {
            let _ = fs::remove_file(&staging_path);
            if error.kind() == io::ErrorKind::AlreadyExists && mode == CommitMode::CreateNew {
                Err(AppError::conflict("片段导出目标已出现，请重新确认保存位置"))
            } else {
                Err(AppError::infrastructure_with_source(
                    "片段导出文件未能提交",
                    error,
                ))
            }
        }
        Err(AtomicCommitError::CommitUnknown(_)) => {
            reconcile_unknown_export_commit(&staging_path, &destination, source, max_bytes)
        }
    }
}

/// 精确读回确认后才清理残留 staging；无法确认时保留尚存副本供后续人工检查。
///
/// @author kongweiguang
fn reconcile_unknown_export_commit(
    staging_path: &Path,
    destination: &Path,
    source: &[u8],
    max_bytes: usize,
) -> AppResult<SnippetTransferFileWriteReceipt> {
    if file_matches(destination, source, max_bytes) {
        let _ = fs::remove_file(staging_path);
        Ok(SnippetTransferFileWriteReceipt {
            bytes_written: source.len(),
            confirmation: SnippetTransferFileConfirmation::ReadBackConfirmed,
        })
    } else {
        Err(AppError::outcome_unknown(
            "片段导出结果未知，请检查所选文件后再决定下一步",
        ))
    }
}

/// 创建唯一 staging 文件，保留系统默认权限并避免覆盖已有用户文件。
fn create_staging_file(parent: &Path) -> AppResult<(PathBuf, File)> {
    for _ in 0..8 {
        let staging = parent.join(format!(
            ".kerminal-snippets-{}.tmp",
            Uuid::new_v4().simple()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
        {
            Ok(file) => return Ok((staging, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "无法创建片段导出暂存文件",
                    source,
                ));
            }
        }
    }
    Err(AppError::infrastructure("无法分配唯一片段导出暂存文件"))
}

/// 只有普通目标的字节与导出正文完全一致时，才将未知提交收敛为可证明成功。
fn file_matches(path: &Path, expected: &[u8], max_bytes: usize) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes as u64
    {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(limit) = u64::try_from(max_bytes.saturating_add(1)) else {
        return false;
    };
    let mut actual = Vec::with_capacity(metadata.len() as usize);
    file.take(limit)
        .read_to_end(&mut actual)
        .is_ok_and(|_| actual.len() <= max_bytes && actual.as_slice() == expected)
}

#[cfg(test)]
#[path = "../../tests/unit/snippet_transfer_file.rs"]
mod tests;
