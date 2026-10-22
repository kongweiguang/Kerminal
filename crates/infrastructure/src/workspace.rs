//! `workspace/session.json` 的当前 v3 读取分类和 CAS 原子写适配器。
//!
//! @author kongweiguang

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kerminal_application::workspace::{
    WorkspaceSessionDocument, WorkspaceSessionExpectedRevision, WorkspaceSessionInvalidReason,
    WorkspaceSessionLoad, WorkspaceSessionRepository, WorkspaceSessionRevision,
};
use kerminal_application::{AppError, AppResult};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::task;
use tokio_util::sync::CancellationToken;

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

const SESSION_RELATIVE_PATH: &str = "workspace/session.json";

/// 唯一 session 文件仓储；读操作仍可用于只读恢复界面，写操作使用启动门禁和 CAS。
#[derive(Clone)]
pub struct WorkspaceSessionStore {
    root: PathBuf,
    storage: StorageCoordinator,
    writable: bool,
}

impl WorkspaceSessionStore {
    /// 复用配置仓储的用户根和共享锁，避免 workspace 与 profile/vault 更新互相覆盖。
    pub fn new(storage: StorageCoordinator, writable: bool) -> Self {
        Self {
            root: storage.root().to_path_buf(),
            storage,
            writable,
        }
    }

    /// 返回实际用户数据根供 desktop 装配诊断使用，不暴露可拼接的 workspace 相对路径。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 在锁内读取原始字节；目录、文件读取遵循既有配置对 symlink 的兼容语义。
    fn load_locked(&self, cancellation: &CancellationToken) -> AppResult<WorkspaceSessionLoad> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("工作区读取已取消"));
        }
        let _lock = self
            .storage
            .lock()
            .map_err(|source| AppError::infrastructure_with_source("无法锁定工作区存储", source))?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("工作区读取已取消"));
        }
        let path = self.root.join(SESSION_RELATIVE_PATH);
        let Some(bytes) = read_regular_file_following_symlink(&path).map_err(|source| {
            AppError::infrastructure_with_source("读取工作区 session 失败", source)
        })?
        else {
            return Ok(WorkspaceSessionLoad::Missing);
        };
        Ok(classify_session_bytes(&bytes))
    }

    /// 已经开始写入时仍按最终目标状态确认 CAS，缺失只允许 no-clobber 创建。
    fn save_locked(
        &self,
        document: &WorkspaceSessionDocument,
        expected: WorkspaceSessionExpectedRevision,
        cancellation: &CancellationToken,
    ) -> AppResult<WorkspaceSessionRevision> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("工作区保存已取消"));
        }
        self.ensure_writable()?;
        let document = WorkspaceSessionDocument::try_from_value(document.as_value().clone())
            .map_err(map_invalid_document)?;
        let bytes = serde_json::to_vec(document.as_value()).map_err(|source| {
            AppError::infrastructure_with_source("编码工作区 session 失败", source)
        })?;
        let path = self.root.join(SESSION_RELATIVE_PATH);
        let _lock = self
            .storage
            .lock()
            .map_err(|source| AppError::infrastructure_with_source("无法锁定工作区存储", source))?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("工作区保存已取消"));
        }
        let current = read_regular_file_following_symlink(&path).map_err(|source| {
            AppError::infrastructure_with_source("读取工作区 session 失败", source)
        })?;
        // 先检查已有内容类型，及时拒绝损坏或未来版本；提交前闭包仍会重复 CAS。
        commit_mode_for_expected(&path, current.as_deref(), &expected)?;

        stage_and_commit(&path, &bytes, cancellation, || {
            if final_path_is_symlink(&path).map_err(|source| {
                AppError::infrastructure_with_source("检查工作区 session 目标失败", source)
            })? {
                return Err(AppError::conflict(
                    "符号链接工作区 session 可以读取，但不能覆盖",
                ));
            }
            let latest = read_regular_file_following_symlink(&path).map_err(|source| {
                AppError::infrastructure_with_source("重新读取工作区 session 失败", source)
            })?;
            commit_mode_for_expected(&path, latest.as_deref(), &expected)
        })?;
        Ok(revision_for_bytes(&bytes))
    }

    /// 只读恢复态由仓储层强制，避免 UI 按钮状态成为唯一写保护。
    fn ensure_writable(&self) -> AppResult<()> {
        if self.writable {
            Ok(())
        } else {
            Err(AppError::configuration_corrupt("工作区 session")
                .with_source(WorkspaceWriteBlocked))
        }
    }
}

#[async_trait]
impl WorkspaceSessionRepository for WorkspaceSessionStore {
    /// 阻塞文件读取移入 Tokio blocking pool；协作取消检查不会触碰持久化内容。
    async fn load(&self, cancellation: CancellationToken) -> AppResult<WorkspaceSessionLoad> {
        let store = self.clone();
        task::spawn_blocking(move || store.load_locked(&cancellation))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("读取工作区 session 任务意外终止", source)
            })?
    }

    /// 仅写入已验证 v3 文档，并将 Missing 与已有 revision 分别映射到 no-clobber/create 或 replace。
    async fn save(
        &self,
        cancellation: CancellationToken,
        document: &WorkspaceSessionDocument,
        expected_revision: WorkspaceSessionExpectedRevision,
    ) -> AppResult<WorkspaceSessionRevision> {
        let store = self.clone();
        let document = document.clone();
        task::spawn_blocking(move || store.save_locked(&document, expected_revision, &cancellation))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("保存工作区 session 任务意外终止", source)
            })?
    }
}

/// 只接受当前 v3；旧版、不完整版本和损坏数据均保留在磁盘供独立迁移或恢复。
fn classify_session_bytes(bytes: &[u8]) -> WorkspaceSessionLoad {
    let revision = revision_for_bytes(bytes);
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => {
            return WorkspaceSessionLoad::Invalid {
                reason: WorkspaceSessionInvalidReason::MalformedJson,
            };
        }
    };
    let Some(object) = value.as_object() else {
        return WorkspaceSessionLoad::Invalid {
            reason: WorkspaceSessionInvalidReason::InvalidRoot,
        };
    };
    match object.get("version").and_then(Value::as_u64) {
        Some(3) => match WorkspaceSessionDocument::try_from_value(value) {
            Ok(document) => WorkspaceSessionLoad::Loaded { document, revision },
            Err(reason) => WorkspaceSessionLoad::Invalid { reason },
        },
        Some(version @ (1 | 2)) => WorkspaceSessionLoad::Unsupported { version },
        Some(version) if version > 3 => WorkspaceSessionLoad::Unsupported { version },
        _ => WorkspaceSessionLoad::Invalid {
            reason: WorkspaceSessionInvalidReason::InvalidSchema,
        },
    }
}

/// 依据持锁时的目标字节决定原子操作，不允许 revision 缺失语义覆盖已有文件。
fn commit_mode_for_expected(
    path: &Path,
    current: Option<&[u8]>,
    expected: &WorkspaceSessionExpectedRevision,
) -> AppResult<CommitMode> {
    if final_path_is_symlink(path).map_err(|source| {
        AppError::infrastructure_with_source("检查工作区 session 目标失败", source)
    })? {
        return Err(AppError::conflict(
            "符号链接工作区 session 可以读取，但不能覆盖",
        ));
    }
    match (expected, current) {
        (WorkspaceSessionExpectedRevision::Missing, None) => Ok(CommitMode::CreateNew),
        (WorkspaceSessionExpectedRevision::Revision(expected), Some(current))
            if revision_for_bytes(current).as_str() == expected.as_str() =>
        {
            match classify_session_bytes(current) {
                WorkspaceSessionLoad::Loaded { .. } => Ok(CommitMode::ReplaceExisting),
                WorkspaceSessionLoad::Invalid { .. } => {
                    Err(AppError::configuration_corrupt("工作区 session 格式无效"))
                }
                WorkspaceSessionLoad::Unsupported { .. } => Err(AppError::unsupported(
                    "此版本工作区 session 不能由当前版本覆盖",
                )),
                WorkspaceSessionLoad::Missing => Err(AppError::conflict(
                    "工作区 session 已被其它操作修改，请重新读取后保存",
                )),
            }
        }
        _ => Err(AppError::conflict(
            "工作区 session 已被其它操作修改，请重新读取后保存",
        )),
    }
}

/// 将完整 staging 文件交给共享平台原语提交，统一保持 no-clobber、替换与目录同步语义。
fn stage_and_commit(
    destination: &Path,
    bytes: &[u8],
    cancellation: &CancellationToken,
    before_commit: impl FnOnce() -> AppResult<CommitMode>,
) -> AppResult<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::invalid("工作区 session 路径无效"))?;
    fs::create_dir_all(parent)
        .map_err(|source| AppError::infrastructure_with_source("创建工作区目录失败", source))?;
    let mut temp_name = destination
        .file_name()
        .ok_or_else(|| AppError::invalid("工作区 session 路径无效"))?
        .to_os_string();
    temp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temp_path = parent.join(temp_name);
    let result = (|| {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp_path)
        }
        .map_err(|source| AppError::infrastructure_with_source("创建工作区暂存文件失败", source))?;
        #[cfg(not(unix))]
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| {
                AppError::infrastructure_with_source("创建工作区暂存文件失败", source)
            })?;

        file.write_all(bytes).map_err(|source| {
            AppError::infrastructure_with_source("写入工作区暂存文件失败", source)
        })?;
        file.sync_all().map_err(|source| {
            AppError::infrastructure_with_source("同步工作区暂存文件失败", source)
        })?;
        drop(file);
        let mode = before_commit()?;
        // 在原子替换调用前最后检查取消；之后必须等待完整 commit receipt 或 unknown。
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("工作区写入已在提交前取消"));
        }
        commit_staged_file(&temp_path, destination, mode).map_err(|source| match source {
            AtomicCommitError::NotCommitted(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) =>
            {
                AppError::conflict("工作区 session 文件状态已变化，请重新读取后保存")
                    .with_source(error)
            }
            AtomicCommitError::NotCommitted(error) => {
                AppError::infrastructure_with_source("提交工作区 session 失败", error)
            }
            AtomicCommitError::CommitUnknown(error) => AppError::outcome_unknown_with_source(
                "工作区 session 提交结果未知，请重新读取",
                error,
            ),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// 读取普通文件并跟随目标 symlink，保持现行 workspace 配置路径可读语义。
fn read_regular_file_following_symlink(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = if link_metadata.file_type().is_symlink() {
        fs::metadata(path)?
    } else {
        link_metadata
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace session target is not a regular file",
        ));
    }
    fs::read(path).map(Some)
}

/// 写入时只拒绝最终路径 symlink，不收紧父目录 symlink 的既有用法。
fn final_path_is_symlink(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Revision 绑定实际文件字节，保留 comments/空白不参与解码造成的内容差异。
fn revision_for_bytes(bytes: &[u8]) -> WorkspaceSessionRevision {
    let digest = Sha256::digest(bytes);
    let value = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    WorkspaceSessionRevision::new(value)
}

/// 将不可展示的 workspace 格式错误留在应用安全码下，不向 UI 暴露原始 JSON 字段。
fn map_invalid_document(reason: WorkspaceSessionInvalidReason) -> AppError {
    match reason {
        WorkspaceSessionInvalidReason::CredentialMaterial => {
            AppError::configuration_corrupt("工作区 session 含凭据正文")
        }
        WorkspaceSessionInvalidReason::MalformedJson
        | WorkspaceSessionInvalidReason::InvalidRoot
        | WorkspaceSessionInvalidReason::InvalidSchema => {
            AppError::configuration_corrupt("工作区 session 格式无效")
        }
    }
}

/// 标记只读恢复写保护的诊断原因，内容不会进入稳定公开错误文案。
#[derive(Debug)]
struct WorkspaceWriteBlocked;

impl std::fmt::Display for WorkspaceWriteBlocked {
    /// 说明写入被启动恢复门禁阻断，不回显用户路径或数据内容。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("workspace persistence is disabled during recovery")
    }
}

impl std::error::Error for WorkspaceWriteBlocked {}
