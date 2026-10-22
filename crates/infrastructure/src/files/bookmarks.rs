//! 路径书签的版本化 JSON 存储；只接受配置根目录注入，不自行推导真实用户目录。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};
use async_trait::async_trait;
use kerminal_application::{AppError, AppResult, files::FileBookmarkRepository};
use kerminal_domain::files::{BookmarkTarget, FileBookmark};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const BOOKMARK_SCHEMA_VERSION: u32 = 1;
const BOOKMARK_LIMIT_PER_TARGET: usize = 100;
const MAX_BOOKMARK_PATH_BYTES: usize = 4_096;
const MAX_BOOKMARK_LABEL_CHARS: usize = 200;

/// 书签持久化复用旧版 schema 和目标 tag，仓储句柄共享写锁避免进程内读改写丢更新。
#[derive(Clone)]
pub struct JsonFileBookmarkRepository {
    path: Arc<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BookmarkDocument {
    bookmarks: Vec<FileBookmark>,
    schema_version: u32,
}

impl JsonFileBookmarkRepository {
    /// 将工作区配置根转换为固定数据路径；测试通过临时目录隔离，不触碰真实用户数据。
    pub fn new(config_root: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(config_root.into().join("data").join("sftp-bookmarks.json")),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// 读取并校验完整文档；损坏、未知字段或未来版本均阻止写入覆盖原数据。
    async fn read_document(&self, cancellation: &CancellationToken) -> AppResult<BookmarkDocument> {
        let raw = match tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("书签读取已取消")),
            result = fs::read_to_string(self.path.as_ref()) => result,
        } {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty_document()),
            Err(error) => return Err(map_io_error(error)),
        };
        let document: BookmarkDocument = serde_json::from_str(&raw)
            .map_err(|_| AppError::configuration_corrupt("SFTP 路径书签"))?;
        validate_document(&document)?;
        Ok(document)
    }

    /// 先完整编码到同目录临时文件并刷盘，再原子替换数据文件以避免半份 JSON。
    async fn write_document(
        &self,
        document: &BookmarkDocument,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        validate_document(document)?;
        ensure_not_cancelled(cancellation)?;
        let mut encoded = serde_json::to_vec_pretty(document)
            .map_err(|_| AppError::infrastructure("SFTP 路径书签无法编码"))?;
        encoded.push(b'\n');
        let parent = self
            .path
            .parent()
            .ok_or_else(|| AppError::invalid("SFTP 书签文件路径无效"))?;
        fs::create_dir_all(parent).await.map_err(map_io_error)?;
        ensure_not_cancelled(cancellation)?;
        let temporary = temporary_sibling(self.path.as_ref());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(map_io_error)?;
        let write_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            result = file.write_all(&encoded) => Some(result),
        };
        match write_result {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                drop(file);
                cleanup_temporary(&temporary).await;
                return Err(map_io_error(error));
            }
            None => {
                drop(file);
                cleanup_temporary(&temporary).await;
                return Err(AppError::cancelled("书签更新已取消"));
            }
        }
        let sync_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            result = file.sync_all() => Some(result),
        };
        match sync_result {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                drop(file);
                cleanup_temporary(&temporary).await;
                return Err(map_io_error(error));
            }
            None => {
                drop(file);
                cleanup_temporary(&temporary).await;
                return Err(AppError::cancelled("书签更新已取消"));
            }
        }
        drop(file);
        ensure_before_commit(cancellation, &temporary).await?;
        let mode = match fs::symlink_metadata(self.path.as_ref()).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                cleanup_temporary(&temporary).await;
                return Err(AppError::conflict("SFTP 书签目标不是普通文件"));
            }
            Ok(_) => CommitMode::ReplaceExisting,
            Err(error) if error.kind() == io::ErrorKind::NotFound => CommitMode::CreateNew,
            Err(error) => {
                cleanup_temporary(&temporary).await;
                return Err(map_io_error(error));
            }
        };
        ensure_before_commit(cancellation, &temporary).await?;
        let source = absolute_path(&temporary).map_err(map_io_error)?;
        let destination = absolute_path(self.path.as_ref()).map_err(map_io_error)?;
        let commit =
            tokio::task::spawn_blocking(move || commit_staged_file(&source, &destination, mode))
                .await
                .map_err(|error| {
                    AppError::outcome_unknown_with_source(
                        "SFTP 书签提交任务结果未知，请重新读取确认",
                        error,
                    )
                })?;
        if let Err(error) = commit {
            cleanup_temporary(&temporary).await;
            return Err(map_commit_error(error));
        }
        Ok(())
    }
}

#[async_trait]
impl FileBookmarkRepository for JsonFileBookmarkRepository {
    /// 返回目标书签的最近收藏优先视图，损坏数据按恢复错误返回而非伪装为空。
    async fn list(
        &self,
        cancellation: CancellationToken,
        target: BookmarkTarget,
    ) -> AppResult<Vec<FileBookmark>> {
        ensure_not_cancelled(&cancellation)?;
        validate_target(&target)?;
        let _guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("书签读取已取消")),
            guard = self.write_lock.lock() => guard,
        };
        let document = self.read_document(&cancellation).await?;
        let mut bookmarks = document
            .bookmarks
            .into_iter()
            .filter(|bookmark| bookmark.target == target)
            .collect::<Vec<_>>();
        sort_bookmarks(&mut bookmarks);
        Ok(bookmarks)
    }

    /// 收藏或取消收藏是幂等状态赋值；每个目标最多一百条并保留旧 schema 字段。
    async fn set(
        &self,
        cancellation: CancellationToken,
        mut bookmark: FileBookmark,
        bookmarked: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        ensure_not_cancelled(&cancellation)?;
        validate_target(&bookmark.target)?;
        bookmark.path = normalize_bookmark_path(&bookmark.target, &bookmark.path)?;
        bookmark.label = normalize_label(bookmark.label.take())?;
        let _guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("书签更新已取消")),
            guard = self.write_lock.lock() => guard,
        };
        let mut document = self.read_document(&cancellation).await?;
        let existing = document
            .bookmarks
            .iter()
            .position(|item| item.target == bookmark.target && item.path == bookmark.path);
        let changed = if bookmarked {
            if existing.is_some() {
                false
            } else {
                let target_count = document
                    .bookmarks
                    .iter()
                    .filter(|item| item.target == bookmark.target)
                    .count();
                if target_count >= BOOKMARK_LIMIT_PER_TARGET {
                    return Err(AppError::conflict("每个文件目标最多保存 100 条路径书签"));
                }
                bookmark.created_at_unix_ms = current_unix_ms();
                document.bookmarks.push(bookmark.clone());
                true
            }
        } else if let Some(index) = existing {
            document.bookmarks.remove(index);
            true
        } else {
            false
        };
        if changed {
            sort_bookmarks(&mut document.bookmarks);
            self.write_document(&document, &cancellation).await?;
        }
        let mut output = document
            .bookmarks
            .into_iter()
            .filter(|item| item.target == bookmark.target)
            .collect::<Vec<_>>();
        sort_bookmarks(&mut output);
        Ok(output)
    }
}

/// 创建空 schema 1 文档，与已发布配置文件保持相同根字段。
fn empty_document() -> BookmarkDocument {
    BookmarkDocument {
        bookmarks: Vec::new(),
        schema_version: BOOKMARK_SCHEMA_VERSION,
    }
}

/// 校验整个文档中的版本、规范路径、稳定目标和去重约束，拒绝部分恢复后覆盖数据。
fn validate_document(document: &BookmarkDocument) -> AppResult<()> {
    if document.schema_version != BOOKMARK_SCHEMA_VERSION {
        return Err(AppError::configuration_corrupt("SFTP 路径书签"));
    }
    let mut unique = HashSet::new();
    let mut counts = std::collections::HashMap::<BookmarkTarget, usize>::new();
    for bookmark in &document.bookmarks {
        validate_target(&bookmark.target)?;
        if bookmark.created_at_unix_ms < 0
            || normalize_bookmark_path(&bookmark.target, &bookmark.path)? != bookmark.path
        {
            return Err(AppError::configuration_corrupt("SFTP 路径书签"));
        }
        let count = counts.entry(bookmark.target.clone()).or_default();
        *count += 1;
        if *count > BOOKMARK_LIMIT_PER_TARGET
            || !unique.insert((bookmark.target.clone(), bookmark.path.clone()))
        {
            return Err(AppError::configuration_corrupt("SFTP 路径书签"));
        }
        if normalize_label(bookmark.label.clone())? != bookmark.label {
            return Err(AppError::configuration_corrupt("SFTP 路径书签"));
        }
    }
    Ok(())
}

/// 目标身份只包含稳定 ID 和协议类型，拒绝控制字符以及可能被误用为路径段的空值。
fn validate_target(target: &BookmarkTarget) -> AppResult<()> {
    let safe_identifier = |value: &str| {
        !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
    };
    match target {
        BookmarkTarget::Local => Ok(()),
        BookmarkTarget::Remote { host_id } if safe_identifier(host_id.as_str()) => Ok(()),
        BookmarkTarget::Container {
            host_id,
            container_id,
            ..
        } if host_id
            .as_ref()
            .is_none_or(|id| safe_identifier(id.as_str()))
            && safe_identifier(container_id) =>
        {
            Ok(())
        }
        _ => Err(AppError::invalid("文件书签目标无效")),
    }
}

/// SSH 与容器路径沿用旧版 POSIX 规范化；本机路径仅去空白并拒绝控制字符。
fn normalize_bookmark_path(target: &BookmarkTarget, path: &str) -> AppResult<String> {
    if path.chars().any(char::is_control) {
        return Err(AppError::invalid("文件书签路径不能包含控制字符"));
    }
    let normalized = match target {
        BookmarkTarget::Local => path.trim().to_owned(),
        BookmarkTarget::Remote { .. } | BookmarkTarget::Container { .. } => {
            let mut value = path.trim().replace('\\', "/");
            if value.is_empty() {
                value.push('/');
            } else if !value.starts_with('/') {
                value.insert(0, '/');
            }
            while value.contains("//") {
                value = value.replace("//", "/");
            }
            if value.len() > 1 {
                value = value.trim_end_matches('/').to_owned();
            }
            value
        }
    };
    if normalized.is_empty() || normalized.len() > MAX_BOOKMARK_PATH_BYTES {
        return Err(AppError::invalid("文件书签路径长度无效"));
    }
    Ok(normalized)
}

/// 规范显示标签长度并将空白标签归一为 None，防止 UI 输入保存成无意义差异。
fn normalize_label(label: Option<String>) -> AppResult<Option<String>> {
    let Some(label) = label else { return Ok(None) };
    let label = label.trim();
    if label.chars().any(char::is_control) || label.chars().count() > MAX_BOOKMARK_LABEL_CHARS {
        return Err(AppError::invalid("文件书签名称无效"));
    }
    Ok((!label.is_empty()).then(|| label.to_owned()))
}

/// 按最近收藏优先排序，时间相同再用规范路径稳定排序。
fn sort_bookmarks(bookmarks: &mut [FileBookmark]) {
    bookmarks.sort_by(|left, right| {
        right
            .created_at_unix_ms
            .cmp(&left.created_at_unix_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
}

/// 生成 Unix 毫秒时间戳；时钟倒退时落在零并由文档校验拒绝负值。
fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// 临时 JSON 与目标同目录，避免替换跨卷并让重命名保持原子操作。
fn temporary_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bookmarks");
    path.with_file_name(format!(".{name}-{}.partial", Uuid::new_v4()))
}

/// 把相对配置根固定到当前目录，使共享提交 helper 收到稳定的绝对路径。
///
/// @author kongweiguang
fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// 把原子提交的未知结果保留为带 source 的基础设施错误，调用方必须重读书签文件。
///
/// @author kongweiguang
fn map_commit_error(error: AtomicCommitError) -> AppError {
    match error {
        AtomicCommitError::NotCommitted(source) => {
            AppError::infrastructure_with_source("SFTP 书签尚未提交", source)
        }
        AtomicCommitError::CommitUnknown(source) => {
            AppError::outcome_unknown_with_source("SFTP 书签提交结果未知，请重新读取确认", source)
        }
    }
}

/// 在文件事务的可取消阶段尽早终止请求，避免继续读取或创建新的暂存内容。
///
/// @author kongweiguang
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("书签操作已取消"))
    } else {
        Ok(())
    }
}

/// 只在原子提交启动之前响应取消；提交一旦移交后台线程就必须等待确定结果。
///
/// @author kongweiguang
async fn ensure_before_commit(cancellation: &CancellationToken, temporary: &Path) -> AppResult<()> {
    if cancellation.is_cancelled() {
        cleanup_temporary(temporary).await;
        Err(AppError::cancelled("书签更新已取消"))
    } else {
        Ok(())
    }
}

/// 清理临时文件失败时只记录错误类型，不覆盖导致本次提交失败的原始结果。
///
/// @author kongweiguang
async fn cleanup_temporary(path: &Path) {
    if let Err(error) = fs::remove_file(path).await
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(error_kind = ?error.kind(), "书签临时文件清理失败");
    }
}

/// 将文件系统错误保留为内部 source，公开错误只呈现稳定的安全文案。
fn map_io_error(error: io::Error) -> AppError {
    match error.kind() {
        io::ErrorKind::PermissionDenied => AppError::conflict("SFTP 书签文件权限不足"),
        _ => AppError::infrastructure_with_source("SFTP 书签存储失败", error),
    }
}
