//! 单一插件状态文件的 revision 与内容联合 CAS。
//! @author kongweiguang

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use kerminal_domain::plugins::{PendingPluginChange, PluginStoreState};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::storage::{
    AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file,
    remove_file_with_parent_sync,
};

const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RECOVERY_BYTES: u64 = MAX_STATE_BYTES + 4096;
const MAX_RECOVERIES_PER_LIST: usize = 1024;
const MAX_PLUGINS: usize = 4096;
const MAX_PENDING: usize = 16384;

/// 明确区分损坏、冲突与未知提交，调用方不得把任何一种当成空状态重试。
#[derive(Debug, Error)]
pub enum PluginStoreError {
    #[error("插件状态文件损坏或不受支持")]
    InvalidState,
    #[error("插件状态已被其他操作修改")]
    Conflict,
    #[error("插件状态提交结果未知，请先读回核对")]
    OutcomeUnknown {
        recovery_path: PathBuf,
        recovery_persisted: bool,
    },
    #[error("插件状态存在待处理的恢复记录")]
    RecoveryRequired { recovery_path: PathBuf },
    #[error("插件恢复记录损坏或不安全")]
    InvalidRecovery { recovery_path: PathBuf },
    #[error("插件恢复记录的准备结果未知，状态尚未提交")]
    RecoveryPreparationUnknown { recovery_path: PathBuf },
    #[error("插件恢复记录清理失败，请先读回核对")]
    RecoveryCleanupFailed {
        recovery_path: PathBuf,
        state_committed: bool,
        deletion_unknown: bool,
        #[source]
        source: io::Error,
    },
    #[error("插件恢复记录的处理结果未知，请先重新检查")]
    RecoveryResolutionUnknown {
        recovery_path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("插件存储不可用")]
    Io(#[source] io::Error),
    #[error("插件存储锁不可用")]
    Lock,
}

/// 摘要保持私有，防止调用方伪造只凭整数 revision 的写入凭据。
#[derive(Clone, Debug)]
pub struct PluginStoreSnapshot {
    pub state: PluginStoreState,
    fingerprint: Option<[u8; 32]>,
}

/// 仅描述读回时可证明的关系；旧记录缺少基线时不能推断是否未提交。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginRecoveryDisposition {
    Applied,
    NotApplied,
    Diverged,
    Indeterminate,
}

/// 检查结果携带私有内容摘要，处理时必须重新核对，不能伪造过期凭据。
#[derive(Clone, Debug)]
pub struct PluginRecoveryInspection {
    path: PathBuf,
    current: PluginStoreSnapshot,
    candidate: PluginStoreState,
    disposition: PluginRecoveryDisposition,
    fingerprint: [u8; 32],
}

impl PluginRecoveryInspection {
    /// 恢复路径来自存储目录，调用方应使用该路径重新发现而非自行拼接。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 返回检查时的权威快照，以便调用方核对实际生效状态。
    pub fn current(&self) -> &PluginStoreSnapshot {
        &self.current
    }

    /// 候选仅供比较，显式处理不会把它写入权威文件。
    pub fn candidate(&self) -> &PluginStoreState {
        &self.candidate
    }

    /// 给出检查时可证明的关系，后续处理仍会重新验证内容。
    pub fn disposition(&self) -> PluginRecoveryDisposition {
        self.disposition
    }
}

/// 已知缺失与旧记录没有基线证据是不同的情况，不能使用同一个 Option 表达。
enum RecoveryBaseline {
    Known(Option<[u8; 32]>),
    Legacy,
}

/// 当前格式严格拒绝额外和重复字段；缺失基线不能被误解为首次创建。
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryDocumentV1 {
    recovery_schema: u32,
    #[serde(deserialize_with = "deserialize_baseline")]
    base_fingerprint: Option<[u8; 32]>,
    candidate: PluginStoreState,
}

/// 兼容旧版只有候选状态的记录，同时让每个分支各自进行严格字段校验。
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RecoveryDocument {
    Current(RecoveryDocumentV1),
    Legacy(PluginStoreState),
}

/// 要求基线字段显式存在，null 才表示提交前文件缺失。
fn deserialize_baseline<'de, D>(deserializer: D) -> Result<Option<[u8; 32]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<[u8; 32]> as serde::Deserialize>::deserialize(deserializer)
}

/// 文件 owner 与其他配置仓储共享同一个 StorageCoordinator 锁。
#[derive(Clone)]
pub struct PluginStore {
    storage: StorageCoordinator,
}

impl PluginStore {
    /// 相对路径只由宿主定义，安装器不能通过插件元数据替换状态文件位置。
    pub fn new(storage: StorageCoordinator) -> Result<Self, PluginStoreError> {
        if !storage.root().is_absolute() {
            return Err(PluginStoreError::InvalidState);
        }
        Ok(Self { storage })
    }

    /// 暴露相同配置根，供启动快照和安装器定位受约束的插件目录。
    pub fn root(&self) -> &Path {
        self.storage.root()
    }

    /// 缺失文件与损坏文件分别处理，失败时不创建或覆盖状态。
    pub fn load(&self) -> Result<PluginStoreSnapshot, PluginStoreError> {
        let _guard = self.storage.lock().map_err(|_| PluginStoreError::Lock)?;
        self.read_unlocked()
    }

    /// 每批最多返回固定数量，逐条处理后可重复发现，避免异常目录耗尽内存。
    pub fn list_recoveries(&self) -> Result<Vec<PathBuf>, PluginStoreError> {
        let _guard = self.storage.lock().map_err(|_| PluginStoreError::Lock)?;
        self.find_recoveries_unlocked(MAX_RECOVERIES_PER_LIST)
    }

    /// 同时读回权威状态与恢复材料，旧格式在缺少基线时只给出保守结论。
    pub fn inspect_recovery(
        &self,
        path: &Path,
    ) -> Result<PluginRecoveryInspection, PluginStoreError> {
        let _guard = self.storage.lock().map_err(|_| PluginStoreError::Lock)?;
        self.inspect_recovery_unlocked(path)
    }

    /// 显式接受当前权威状态并移除已核对的记录；绝不重放或覆盖候选。
    pub fn resolve_recovery(
        &self,
        inspected: &PluginRecoveryInspection,
    ) -> Result<PluginStoreSnapshot, PluginStoreError> {
        let _guard = self.storage.lock().map_err(|_| PluginStoreError::Lock)?;
        let latest = self.inspect_recovery_unlocked(&inspected.path)?;
        if latest.fingerprint != inspected.fingerprint
            || latest.current.fingerprint != inspected.current.fingerprint
            || latest.current.state != inspected.current.state
        {
            return Err(PluginStoreError::Conflict);
        }
        match remove_file_with_parent_sync(&inspected.path) {
            Ok(()) => Ok(latest.current),
            Err(AtomicCommitError::NotCommitted(source)) => Err(PluginStoreError::Io(source)),
            Err(AtomicCommitError::CommitUnknown(source)) => {
                Err(PluginStoreError::RecoveryResolutionUnknown {
                    recovery_path: inspected.path.clone(),
                    source,
                })
            }
        }
    }

    /// 清理期间持有共享锁并重新读状态，避免删除刚被其他存储操作引用的版本。
    pub(super) fn with_locked_state<T>(
        &self,
        operation: impl FnOnce(&PluginStoreState) -> T,
    ) -> Result<T, PluginStoreError> {
        let _guard = self.storage.lock().map_err(|_| PluginStoreError::Lock)?;
        self.require_no_recovery_unlocked()?;
        let current = self.read_unlocked()?;
        Ok(operation(&current.state))
    }

    /// 仅在内容与 revision 均匹配时提交；未知结果向上保留读回义务。
    pub fn compare_and_swap(
        &self,
        expected: &PluginStoreSnapshot,
        next: PluginStoreState,
    ) -> Result<PluginStoreSnapshot, PluginStoreError> {
        self.publish_and_compare(expected, next, || Ok(()))
    }

    /// 版本发布与状态 CAS 共用同一锁；清理不能介入发布和 pending 提交之间。
    pub(super) fn publish_and_compare(
        &self,
        expected: &PluginStoreSnapshot,
        mut next: PluginStoreState,
        publish: impl FnOnce() -> Result<(), PluginStoreError>,
    ) -> Result<PluginStoreSnapshot, PluginStoreError> {
        let _guard = self.storage.lock().map_err(|_| PluginStoreError::Lock)?;
        self.require_no_recovery_unlocked()?;
        let current = self.read_unlocked()?;
        if current.fingerprint != expected.fingerprint || current.state != expected.state {
            return Err(PluginStoreError::Conflict);
        }
        next.revision = current
            .state
            .revision
            .checked_add(1)
            .ok_or(PluginStoreError::InvalidState)?;
        validate_state(&next)?;
        let bytes = serde_json::to_vec_pretty(&next).map_err(|_| PluginStoreError::InvalidState)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(PluginStoreError::InvalidState);
        }
        let directory = self.plugin_directory(true)?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(&directory).map_err(PluginStoreError::Io)?;
        temporary.write_all(&bytes).map_err(PluginStoreError::Io)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(PluginStoreError::Io)?;
        let temporary = temporary.into_temp_path();
        let latest = self.read_unlocked()?;
        if latest.fingerprint != expected.fingerprint || latest.state != expected.state {
            return Err(PluginStoreError::Conflict);
        }
        let mode = if latest.fingerprint.is_some() {
            CommitMode::ReplaceExisting
        } else {
            CommitMode::CreateNew
        };
        publish()?;
        commit_with_recovery(
            &directory,
            &temporary,
            mode,
            latest.fingerprint,
            &next,
            commit_staged_file,
        )?;
        Ok(PluginStoreSnapshot {
            state: next,
            fingerprint: Some(Sha256::digest(&bytes).into()),
        })
    }

    /// 状态写入和版本清理必须先经过恢复门禁，即使当前权威文件仍可读取。
    fn require_no_recovery_unlocked(&self) -> Result<(), PluginStoreError> {
        if let Some(recovery_path) = self.find_recoveries_unlocked(1)?.into_iter().next() {
            return Err(PluginStoreError::RecoveryRequired { recovery_path });
        }
        Ok(())
    }

    /// 文件类型和内容暂不筛选，以免损坏文件或重解析点绕过写入门禁。
    fn find_recoveries_unlocked(&self, limit: usize) -> Result<Vec<PathBuf>, PluginStoreError> {
        let directory = match self.plugin_directory(false) {
            Ok(directory) => directory,
            Err(PluginStoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        for entry in fs::read_dir(directory).map_err(PluginStoreError::Io)? {
            let entry = entry.map_err(PluginStoreError::Io)?;
            if is_recovery_name(&entry.file_name()) {
                paths.push(entry.path());
                if paths.len() == limit {
                    break;
                }
            }
        }
        paths.sort();
        Ok(paths)
    }

    /// 固定目录、文件类型和大小均须满足约束；无法识别的内容保持原样并阻断写入。
    fn inspect_recovery_unlocked(
        &self,
        path: &Path,
    ) -> Result<PluginRecoveryInspection, PluginStoreError> {
        let directory = self.plugin_directory(false)?;
        let invalid = || PluginStoreError::InvalidRecovery {
            recovery_path: path.to_path_buf(),
        };
        if path.parent() != Some(directory.as_path())
            || !path.file_name().is_some_and(is_recovery_name)
        {
            return Err(invalid());
        }
        let metadata = fs::symlink_metadata(path).map_err(PluginStoreError::Io)?;
        if !metadata.is_file() || !safe_metadata(&metadata) || metadata.len() > MAX_RECOVERY_BYTES {
            return Err(invalid());
        }
        let file = open_read_no_follow(path).map_err(PluginStoreError::Io)?;
        let opened = file.metadata().map_err(PluginStoreError::Io)?;
        if !opened.is_file() || !safe_metadata(&opened) || opened.len() > MAX_RECOVERY_BYTES {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECOVERY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(PluginStoreError::Io)?;
        if bytes.len() as u64 > MAX_RECOVERY_BYTES {
            return Err(invalid());
        }
        let (candidate, baseline) = parse_recovery(&bytes).ok_or_else(invalid)?;
        let current = self.read_unlocked()?;
        let disposition = if current.fingerprint.is_some() && current.state == candidate {
            PluginRecoveryDisposition::Applied
        } else {
            match baseline {
                RecoveryBaseline::Known(fingerprint) if current.fingerprint == fingerprint => {
                    PluginRecoveryDisposition::NotApplied
                }
                RecoveryBaseline::Known(_) => PluginRecoveryDisposition::Diverged,
                RecoveryBaseline::Legacy => PluginRecoveryDisposition::Indeterminate,
            }
        };
        Ok(PluginRecoveryInspection {
            path: path.to_path_buf(),
            current,
            candidate,
            disposition,
            fingerprint: Sha256::digest(&bytes).into(),
        })
    }

    /// 仅接受普通目录；符号链接或重解析点不能把状态文件重定向到配置根之外。
    fn plugin_directory(&self, create: bool) -> Result<PathBuf, PluginStoreError> {
        let directory = self.storage.root().join("plugins");
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && safe_metadata(&metadata) => Ok(directory),
            Ok(_) => Err(PluginStoreError::InvalidState),
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                fs::create_dir(&directory).map_err(PluginStoreError::Io)?;
                Ok(directory)
            }
            Err(error) => Err(PluginStoreError::Io(error)),
        }
    }

    /// 限制字节数并检查普通文件属性，避免异常文件被误当成首次安装。
    pub(super) fn read_unlocked(&self) -> Result<PluginStoreSnapshot, PluginStoreError> {
        let directory = match self.plugin_directory(false) {
            Ok(directory) => directory,
            Err(PluginStoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(empty_snapshot());
            }
            Err(error) => return Err(error),
        };
        let path = directory.join("state.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty_snapshot()),
            Err(error) => return Err(PluginStoreError::Io(error)),
        };
        if !metadata.is_file() || !safe_metadata(&metadata) || metadata.len() > MAX_STATE_BYTES {
            return Err(PluginStoreError::InvalidState);
        }
        let file = open_read_no_follow(&path).map_err(PluginStoreError::Io)?;
        let opened = file.metadata().map_err(PluginStoreError::Io)?;
        if !opened.is_file() || !safe_metadata(&opened) || opened.len() > MAX_STATE_BYTES {
            return Err(PluginStoreError::InvalidState);
        }
        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(PluginStoreError::Io)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(PluginStoreError::InvalidState);
        }
        let state: PluginStoreState =
            serde_json::from_slice(&bytes).map_err(|_| PluginStoreError::InvalidState)?;
        validate_state(&state)?;
        Ok(PluginStoreSnapshot {
            state,
            fingerprint: Some(Sha256::digest(&bytes).into()),
        })
    }
}

/// 先持久发布基线与候选，进程中断后也能发现记录；状态提交未知时绝不清理它。
fn commit_with_recovery(
    directory: &Path,
    staged: &Path,
    mode: CommitMode,
    baseline: Option<[u8; 32]>,
    candidate: &PluginStoreState,
    commit: impl FnOnce(&Path, &Path, CommitMode) -> Result<(), AtomicCommitError>,
) -> Result<(), PluginStoreError> {
    let recovery_path = prepare_recovery(directory, baseline, candidate, commit_staged_file)?;
    let result = match commit(staged, &directory.join("state.json"), mode) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::NotCommitted(source)) => Err(source),
        Err(AtomicCommitError::CommitUnknown(_)) => {
            return Err(PluginStoreError::OutcomeUnknown {
                recovery_path,
                recovery_persisted: true,
            });
        }
    };
    finish_recovery_cleanup(result, recovery_path, remove_file_with_parent_sync)
}

/// 恢复材料自身先经过原子发布和目录同步，准备结果未知时不执行状态提交。
fn prepare_recovery(
    directory: &Path,
    baseline: Option<[u8; 32]>,
    candidate: &PluginStoreState,
    publish: impl FnOnce(&Path, &Path, CommitMode) -> Result<(), AtomicCommitError>,
) -> Result<PathBuf, PluginStoreError> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "recovery_schema": 1,
        "base_fingerprint": baseline,
        "candidate": candidate,
    }))
    .map_err(|_| PluginStoreError::InvalidState)?;
    if bytes.len() as u64 > MAX_RECOVERY_BYTES {
        return Err(PluginStoreError::InvalidState);
    }
    let mut recovery = tempfile::Builder::new()
        .prefix(".plugin-recovery-stage-")
        .tempfile_in(directory)
        .map_err(PluginStoreError::Io)?;
    recovery.write_all(&bytes).map_err(PluginStoreError::Io)?;
    recovery
        .as_file()
        .sync_all()
        .map_err(PluginStoreError::Io)?;
    // keep 清除 Windows TEMPORARY 属性；TempPath 只负责清理尚未发布的暂存文件。
    let (file, staged_path) = recovery
        .keep()
        .map_err(|error| PluginStoreError::Io(error.error))?;
    drop(file);
    let recovery_stage =
        tempfile::TempPath::try_from_path(staged_path).map_err(PluginStoreError::Io)?;
    let recovery_path = directory.join(format!("state-recovery-{}.json", uuid::Uuid::new_v4()));
    match publish(&recovery_stage, &recovery_path, CommitMode::CreateNew) {
        Ok(()) => Ok(recovery_path),
        Err(AtomicCommitError::NotCommitted(source)) => Err(PluginStoreError::Io(source)),
        Err(AtomicCommitError::CommitUnknown(_)) => {
            Err(PluginStoreError::RecoveryPreparationUnknown { recovery_path })
        }
    }
}

/// 状态结果已知但删除失败时分别报告两者，避免把已提交状态当成未提交重试。
fn finish_recovery_cleanup(
    result: Result<(), io::Error>,
    recovery_path: PathBuf,
    remove: impl FnOnce(&Path) -> Result<(), AtomicCommitError>,
) -> Result<(), PluginStoreError> {
    let state_committed = result.is_ok();
    match remove(&recovery_path) {
        Ok(()) => result.map_err(PluginStoreError::Io),
        Err(error) => {
            let (deletion_unknown, source) = match error {
                AtomicCommitError::NotCommitted(source) => (false, source),
                AtomicCommitError::CommitUnknown(source) => (true, source),
            };
            Err(PluginStoreError::RecoveryCleanupFailed {
                recovery_path,
                state_committed,
                deletion_unknown,
                source,
            })
        }
    }
}

/// 名称匹配对 Windows 大小写不敏感，并故意包含损坏、链接及非普通文件。
fn is_recovery_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.starts_with("state-recovery-") && name.ends_with(".json")
}

/// 新格式保存完整基线摘要；旧格式只有候选，不得根据 revision 猜测提交是否发生。
fn parse_recovery(bytes: &[u8]) -> Option<(PluginStoreState, RecoveryBaseline)> {
    let (candidate, baseline) = match serde_json::from_slice(bytes).ok()? {
        RecoveryDocument::Current(record) if record.recovery_schema == 1 => (
            record.candidate,
            RecoveryBaseline::Known(record.base_fingerprint),
        ),
        RecoveryDocument::Current(_) => return None,
        RecoveryDocument::Legacy(candidate) => (candidate, RecoveryBaseline::Legacy),
    };
    validate_state(&candidate).ok()?;
    if serde_json::to_vec_pretty(&candidate).ok()?.len() as u64 > MAX_STATE_BYTES {
        return None;
    }
    Some((candidate, baseline))
}

#[cfg(test)]
#[path = "../../tests/unit/plugin_store_recovery.rs"]
mod tests;

/// 打开最终路径时禁止跟随链接，避免检查元数据后被替换而读取链接目标。
fn open_read_no_follow(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT 令句柄指向重解析点本身，再检查句柄元数据。
        options.custom_flags(0x0020_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Windows 的 junction 也是重解析点，不能只检查普通符号链接。
fn safe_metadata(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        !metadata.file_type().is_symlink() && metadata.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        !metadata.file_type().is_symlink()
    }
}

/// 缺失状态的修订凭据为 None，首次创建必须采用 no-clobber 提交。
fn empty_snapshot() -> PluginStoreSnapshot {
    PluginStoreSnapshot {
        state: PluginStoreState::default(),
        fingerprint: None,
    }
}

/// 磁盘状态中的待安装路径只能引用由身份推导出的版本目录。
pub(crate) fn validate_state(state: &PluginStoreState) -> Result<(), PluginStoreError> {
    if state.schema != 1
        || state.plugins.len() > MAX_PLUGINS
        || state.failures.len() > MAX_PLUGINS
        || state.pending.len() > MAX_PENDING
        || state
            .plugins
            .values()
            .any(|plugin| plugin.installed_at.is_empty() || plugin.installed_at.len() > 64)
        || state.failures.values().any(|message| message.len() > 1024)
        || state
            .boot
            .as_ref()
            .is_some_and(|boot| uuid::Uuid::parse_str(&boot.id).is_err())
    {
        return Err(PluginStoreError::InvalidState);
    }
    for change in &state.pending {
        match change {
            PendingPluginChange::Install {
                id,
                to,
                staged,
                installed_at,
                ..
            } => {
                if staged != &format!("packages/{id}/{to}")
                    || installed_at.is_empty()
                    || installed_at.len() > 64
                {
                    return Err(PluginStoreError::InvalidState);
                }
            }
            PendingPluginChange::Update { id, to, staged }
                if staged != &format!("packages/{id}/{to}") =>
            {
                return Err(PluginStoreError::InvalidState);
            }
            _ => {}
        }
    }
    Ok(())
}
