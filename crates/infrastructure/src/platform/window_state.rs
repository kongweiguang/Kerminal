//! 主窗口恢复状态的本地原子文件适配器。
//!
//! @author kongweiguang

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    platform::window_state::{
        WindowRect, WindowStateLoad, WindowStateRepository, WindowStateSnapshot, WindowStateSource,
        WindowStateUnavailableReason,
    },
};
use serde::{Deserialize, Serialize};

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

const CURRENT_SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 16 * 1024;
const CURRENT_FILE_NAME: &str = "window-state.json";

/// 普通运行态只读写当前格式；旧 Tauri 数据只能由启动前的一次迁移处理。
#[derive(Clone)]
pub struct WindowStateStore {
    storage: StorageCoordinator,
}

impl WindowStateStore {
    /// 只接受当前数据根，不保留旧来源入口，避免新文件缺失时绕过已完成的迁移契约。
    ///
    /// @author kongweiguang
    pub fn new(storage: StorageCoordinator) -> Self {
        Self { storage }
    }

    /// 返回当前 GPUI 独立文件路径，调用方只用于诊断，不应将其展示为用户可配置项。
    pub fn state_file(&self) -> PathBuf {
        self.storage.root().join(CURRENT_FILE_NAME)
    }

    /// 在 Tokio blocking pool 中读文件和锁，避免窗口事件线程等待磁盘或 OS lock。
    async fn load_blocking(&self) -> AppResult<WindowStateLoad> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.load_sync())
            .await
            .map_err(|error| AppError::infrastructure_with_source("窗口状态读取线程失败", error))?
    }

    /// 在同一 blocking pool 事务中复读 revision 并提交 CAS，调用方不持有同步锁跨 await。
    async fn compare_and_swap_blocking(
        &self,
        expected_revision: Option<u64>,
        snapshot: WindowStateSnapshot,
    ) -> AppResult<u64> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.compare_and_swap_sync(expected_revision, snapshot)
        })
        .await
        .map_err(|error| AppError::infrastructure_with_source("窗口状态写入线程失败", error))?
    }

    /// 当前文件缺失明确返回 Missing；迁移后不能再次从旧 Tauri 目录恢复或隐式导入。
    ///
    /// @author kongweiguang
    fn load_sync(&self) -> AppResult<WindowStateLoad> {
        let _lock = match self.storage.lock() {
            Ok(lock) => lock,
            Err(_) => {
                return Ok(WindowStateLoad::Unavailable {
                    reason: WindowStateUnavailableReason::StorageLockUnavailable,
                });
            }
        };
        let current_path = self.state_file();
        match read_current_file(&current_path) {
            Ok(Some(record)) => Ok(WindowStateLoad::Restored {
                snapshot: record.snapshot,
                revision: Some(record.revision),
                source: WindowStateSource::Current,
            }),
            Ok(None) => Ok(WindowStateLoad::Missing),
            Err(ReadStateError::UnsupportedSchemaVersion(version)) => {
                Ok(WindowStateLoad::Unavailable {
                    reason: WindowStateUnavailableReason::UnsupportedSchemaVersion(version),
                })
            }
            Err(ReadStateError::Corrupt) => Ok(WindowStateLoad::Unavailable {
                reason: WindowStateUnavailableReason::Corrupt,
            }),
            Err(ReadStateError::StorageUnavailable) => Ok(WindowStateLoad::Unavailable {
                reason: WindowStateUnavailableReason::StorageUnavailable,
            }),
        }
    }

    /// 在共享根锁内比较 revision、写同目录 staging 并原子替换，冲突与未知提交均不重试。
    fn compare_and_swap_sync(
        &self,
        expected_revision: Option<u64>,
        snapshot: WindowStateSnapshot,
    ) -> AppResult<u64> {
        if !snapshot.normal_bounds.is_valid() {
            return Err(AppError::invalid("窗口状态尺寸超出允许范围"));
        }
        let _lock = self
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("窗口状态存储锁不可用"))?;
        let destination = self.state_file();
        let current = read_current_file(&destination).map_err(read_error_to_app_error)?;
        let actual_revision = current.as_ref().map(|record| record.revision);
        if actual_revision != expected_revision {
            return Err(AppError::conflict("窗口状态已被另一实例修改，请重新读取"));
        }
        let revision = expected_revision
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("窗口状态 revision 已耗尽"))?;
        let document = WindowStateDocument {
            schema_version: CURRENT_SCHEMA_VERSION,
            revision,
            state: PersistedWindowState::from(snapshot),
        };
        let mut staging = tempfile::Builder::new()
            .prefix(".window-state-")
            .suffix(".tmp")
            .tempfile_in(self.storage.root())
            .map_err(|error| {
                AppError::infrastructure_with_source("窗口状态 staging 创建失败", error)
            })?;
        serde_json::to_writer(staging.as_file_mut(), &document)
            .map_err(|error| AppError::infrastructure_with_source("窗口状态序列化失败", error))?;
        staging.as_file_mut().write_all(b"\n").map_err(|error| {
            AppError::infrastructure_with_source("窗口状态 staging 写入失败", error)
        })?;
        staging.as_file_mut().flush().map_err(|error| {
            AppError::infrastructure_with_source("窗口状态 staging 写入失败", error)
        })?;
        let (_, staging_path) = staging.keep().map_err(|error| {
            AppError::infrastructure_with_source("窗口状态 staging 关闭失败", error.error)
        })?;
        let mode = if current.is_some() {
            CommitMode::ReplaceExisting
        } else {
            CommitMode::CreateNew
        };
        match commit_staged_file(&staging_path, &destination, mode) {
            Ok(()) => Ok(revision),
            Err(AtomicCommitError::NotCommitted(error)) => {
                let _ = fs::remove_file(&staging_path);
                Err(AppError::infrastructure_with_source(
                    "窗口状态未能提交",
                    error,
                ))
            }
            Err(AtomicCommitError::CommitUnknown(error)) => {
                Err(AppError::outcome_unknown_with_source(
                    "窗口状态提交结果未知，请重新读取确认",
                    error,
                ))
            }
        }
    }
}

#[async_trait]
impl WindowStateRepository for WindowStateStore {
    /// 把可能阻塞的文件访问和 storage lock 移入 Tokio blocking pool。
    async fn load(&self) -> AppResult<WindowStateLoad> {
        self.load_blocking().await
    }

    /// 用 CAS 防止多个桌面进程彼此覆盖窗口尺寸，并沿用根级原子提交实现。
    async fn compare_and_swap(
        &self,
        expected_revision: Option<u64>,
        snapshot: WindowStateSnapshot,
    ) -> AppResult<u64> {
        self.compare_and_swap_blocking(expected_revision, snapshot)
            .await
    }
}

#[derive(Serialize)]
struct WindowStateDocument {
    schema_version: u32,
    revision: u64,
    state: PersistedWindowState,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct PersistedWindowState {
    normal_bounds: WindowRect,
    maximized: bool,
    fullscreen: bool,
}

impl From<WindowStateSnapshot> for PersistedWindowState {
    /// 保持磁盘 DTO 与 GPUI/API DTO 分离，避免桌面 API 变化意外升级文件格式。
    fn from(snapshot: WindowStateSnapshot) -> Self {
        Self {
            normal_bounds: snapshot.normal_bounds,
            maximized: snapshot.maximized,
            fullscreen: snapshot.fullscreen,
        }
    }
}

impl From<PersistedWindowState> for WindowStateSnapshot {
    /// 只映射已经通过版本、结构和几何验证的本地磁盘 DTO。
    fn from(state: PersistedWindowState) -> Self {
        Self {
            normal_bounds: state.normal_bounds,
            maximized: state.maximized,
            fullscreen: state.fullscreen,
        }
    }
}

#[derive(Deserialize)]
struct WindowStateEnvelope {
    schema_version: u32,
}

#[derive(Deserialize)]
struct DecodedWindowStateDocument {
    schema_version: u32,
    revision: u64,
    state: PersistedWindowState,
}

struct CurrentWindowState {
    revision: u64,
    snapshot: WindowStateSnapshot,
}

#[derive(Debug)]
enum ReadStateError {
    Corrupt,
    UnsupportedSchemaVersion(u32),
    StorageUnavailable,
}

/// 一次迁移复用普通 reader 的当前格式校验；只读探测不取得会落盘的存储锁，也不接受旧来源。
///
/// @author kongweiguang
pub(crate) fn validate_current_window_state(path: &Path) -> AppResult<bool> {
    read_current_file(path)
        .map(|state| state.is_some())
        .map_err(read_error_to_app_error)
}

/// 先探测最小 envelope 的版本，再解析固定 DTO，未来版本包含额外字段时仍正确拒绝。
fn read_current_file(path: &Path) -> Result<Option<CurrentWindowState>, ReadStateError> {
    let Some(bytes) = read_bounded_file(path)? else {
        return Ok(None);
    };
    let envelope = serde_json::from_slice::<WindowStateEnvelope>(&bytes)
        .map_err(|_| ReadStateError::Corrupt)?;
    if envelope.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(ReadStateError::UnsupportedSchemaVersion(
            envelope.schema_version,
        ));
    }
    let document = serde_json::from_slice::<DecodedWindowStateDocument>(&bytes)
        .map_err(|_| ReadStateError::Corrupt)?;
    if document.schema_version != CURRENT_SCHEMA_VERSION
        || document.revision == 0
        || !document.state.normal_bounds.is_valid()
    {
        return Err(ReadStateError::Corrupt);
    }
    Ok(Some(CurrentWindowState {
        revision: document.revision,
        snapshot: document.state.into(),
    }))
}

/// 限制状态文件尺寸并拒绝符号链接，避免启动时跟随用户可控的任意路径。
fn read_bounded_file(path: &Path) -> Result<Option<Vec<u8>>, ReadStateError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ReadStateError::StorageUnavailable),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_STATE_BYTES
    {
        return Err(ReadStateError::Corrupt);
    }
    let file = fs::File::open(path).map_err(|_| ReadStateError::StorageUnavailable)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadStateError::StorageUnavailable)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(ReadStateError::Corrupt);
    }
    Ok(Some(bytes))
}

/// 将底层状态解析错误映射为脱敏应用错误，写入路径和 JSON 内容不进入公开消息。
fn read_error_to_app_error(error: ReadStateError) -> AppError {
    match error {
        ReadStateError::Corrupt => AppError::conflict("窗口状态文件损坏，原内容已保留"),
        ReadStateError::UnsupportedSchemaVersion(_) => {
            AppError::conflict("窗口状态版本暂不支持，原内容已保留")
        }
        ReadStateError::StorageUnavailable => AppError::infrastructure("窗口状态存储不可用"),
    }
}
