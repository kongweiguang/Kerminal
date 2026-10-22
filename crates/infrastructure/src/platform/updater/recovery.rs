//! updater 跨进程恢复记录；安装提交前保存已验签文件与最小状态收据。
//!
//! @author kongweiguang

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use kerminal_application::updates::{UpdateRecoveryId, UpdateRecoveryRecord, UpdateRecoveryState};
use kerminal_application::{AppError, AppResult};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

/// 收据只存身份、版本、SDK target 和摘要，避免跨进程持久化 URL、签名或公钥。
const MAX_RECEIPT_BYTES: u64 = 16 * 1024;

/// 限制损坏或被手工扩大的恢复目录扫描量，超限时保留现场并显式失败。
const MAX_RECOVERY_RECORDS: usize = 16;

const PENDING_RECEIPT: &str = "receipt.pending.json";
const INSTALLING_RECEIPT: &str = "receipt.installing.json";
const AWAITING_RECEIPT: &str = "receipt.awaiting-confirmation.json";
const ARTIFACT_FILE: &str = "verified-update.bin";

/// 明确区分安装尚未启动、SDK 已被调用以及 SDK 已返回成功的持久化阶段。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptState {
    PendingConfirmation,
    Installing,
    AwaitingConfirmation,
}

/// 跨进程收据字段使用严格 schema，避免未知值被误解成安全的旧状态。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryReceipt {
    schema: u8,
    id: String,
    version: String,
    target: String,
    artifact_sha256: String,
    state: ReceiptState,
    installed_target_sha256: Option<String>,
}

/// 恢复目录仅接收 composition root 显式注入的私有配置路径。
#[derive(Clone, Debug)]
pub(super) struct RecoveryStore {
    root: PathBuf,
    max_artifact_bytes: u64,
}

impl RecoveryStore {
    /// 创建并校验私有恢复根；拒绝根目录本身为符号链接以限制删除边界。
    pub(super) fn new(root: PathBuf, max_artifact_bytes: u64) -> AppResult<Self> {
        if max_artifact_bytes == 0 {
            return Err(AppError::invalid("更新恢复包大小限制无效"));
        }
        fs::create_dir_all(&root).map_err(|error| {
            AppError::infrastructure_with_source("初始化更新恢复目录失败", error)
        })?;
        let metadata = fs::symlink_metadata(&root)
            .map_err(|error| AppError::infrastructure_with_source("读取更新恢复目录失败", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::invalid("更新恢复目录必须是普通目录"));
        }
        #[cfg(unix)]
        fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .map_err(|error| AppError::infrastructure_with_source("保护更新恢复目录失败", error))?;
        let root = root
            .canonicalize()
            .map_err(|error| AppError::infrastructure_with_source("解析更新恢复目录失败", error))?;
        Ok(Self {
            root,
            max_artifact_bytes,
        })
    }

    /// 将已验签内存字节直接原子持久化；rename 前关闭 staging 文件句柄以兼容 Windows 目录移动。
    pub(super) fn persist_pending_bytes(
        &self,
        id: UpdateRecoveryId,
        version: &str,
        target: &str,
        verified_artifact: &[u8],
    ) -> AppResult<PathBuf> {
        validate_receipt_values(version, target)?;
        if verified_artifact.is_empty() || verified_artifact.len() as u64 > self.max_artifact_bytes
        {
            return Err(AppError::invalid("已验签更新包大小无效"));
        }
        let record_dir = self.record_dir(id);
        if record_dir.exists() {
            return Err(AppError::conflict("更新恢复记录身份已存在"));
        }
        let staging = Builder::new()
            .prefix(".staging-")
            .tempdir_in(&self.root)
            .map_err(|error| {
                AppError::infrastructure_with_source("创建更新恢复暂存目录失败", error)
            })?;
        #[cfg(unix)]
        fs::set_permissions(
            staging.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .map_err(|error| AppError::infrastructure_with_source("保护更新恢复暂存目录失败", error))?;

        let artifact_path = staging.path().join(ARTIFACT_FILE);
        let mut artifact_file = create_private_file(&artifact_path)
            .map_err(|error| AppError::infrastructure_with_source("创建更新恢复包失败", error))?;
        artifact_file
            .write_all(verified_artifact)
            .map_err(|error| AppError::infrastructure_with_source("写入更新恢复包失败", error))?;
        artifact_file
            .sync_all()
            .map_err(|error| AppError::infrastructure_with_source("同步更新恢复包失败", error))?;
        drop(artifact_file);
        let artifact_digest: [u8; 32] = Sha256::digest(verified_artifact).into();
        self.publish_pending(id, version, target, staging, hex_digest(&artifact_digest))
    }

    /// 写入 receipt 后以 no-clobber、write-through 的目录移动发布完整恢复记录。
    fn publish_pending(
        &self,
        id: UpdateRecoveryId,
        version: &str,
        target: &str,
        staging: tempfile::TempDir,
        artifact_sha256: String,
    ) -> AppResult<PathBuf> {
        let record_dir = self.record_dir(id);
        let receipt = RecoveryReceipt {
            schema: 1,
            id: id.as_uuid().to_string(),
            version: version.to_owned(),
            target: target.to_owned(),
            artifact_sha256,
            state: ReceiptState::PendingConfirmation,
            installed_target_sha256: None,
        };
        write_new_receipt(&staging.path().join(PENDING_RECEIPT), &receipt)
            .map_err(|error| AppError::infrastructure_with_source("写入更新恢复收据失败", error))?;
        sync_directory(staging.path())
            .map_err(|error| AppError::infrastructure_with_source("同步更新恢复目录失败", error))?;
        commit_staged_file(staging.path(), &record_dir, CommitMode::MoveNoReplace).map_err(
            |error| {
                map_recovery_commit_error(error, "提交更新恢复记录失败", "提交更新恢复记录结果未知")
            },
        )?;
        Ok(record_dir)
    }

    /// 在 SDK 调用前提交 installing 收据；目标文件创建完成前绝不进入安装代码。
    pub(super) fn mark_installing(&self, id: UpdateRecoveryId) -> AppResult<()> {
        self.transition_receipt(id, ReceiptState::Installing, None)
    }

    /// 只有 SDK 成功返回后才记录目标文件摘要；artifact 保留到重启核验或用户明确确认。
    pub(super) fn mark_awaiting_confirmation(
        &self,
        id: UpdateRecoveryId,
        installed_target_sha256: Option<String>,
    ) -> AppResult<()> {
        if let Some(digest) = installed_target_sha256.as_deref() {
            validate_digest(digest)?;
        }
        self.transition_receipt(
            id,
            ReceiptState::AwaitingConfirmation,
            installed_target_sha256,
        )
    }

    /// SDK 明确未产生副作用时恢复 pending 状态，保留原句柄供同进程安全重试。
    pub(super) fn restore_pending(&self, id: UpdateRecoveryId) -> AppResult<()> {
        let directory = self.checked_record_dir(id)?;
        let receipt = read_latest_receipt(&directory)?;
        validate_receipt(&receipt, id)?;
        match receipt.state {
            ReceiptState::PendingConfirmation => Ok(()),
            ReceiptState::Installing => {
                self.transition_receipt(id, ReceiptState::PendingConfirmation, None)
            }
            ReceiptState::AwaitingConfirmation => {
                Err(AppError::conflict("安装结果已提交，不能恢复为待安装状态"))
            }
        }
    }

    /// 启动时校验收据和 artifact；只有 SDK 成功返回且目标摘要完全吻合才自动清理。
    pub(super) fn list(
        &self,
        current_version: &str,
        current_target: &str,
        current_target_sha256: Option<&str>,
    ) -> AppResult<Vec<UpdateRecoveryRecord>> {
        let entries = fs::read_dir(&self.root)
            .map_err(|error| AppError::infrastructure_with_source("读取更新恢复目录失败", error))?;
        let mut records = Vec::new();
        let current_version = Version::parse(current_version)
            .map_err(|_| AppError::invalid("当前应用版本不是有效 SemVer"))?;

        for entry in entries {
            let entry = entry.map_err(|error| {
                AppError::infrastructure_with_source("读取更新恢复记录失败", error)
            })?;
            let file_type = entry.file_type().map_err(|error| {
                AppError::infrastructure_with_source("检查更新恢复记录失败", error)
            })?;
            if file_type.is_symlink() {
                return Err(AppError::invalid("更新恢复目录包含符号链接"));
            }
            if !file_type.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().and_then(parse_record_id) else {
                continue;
            };
            if records.len() >= MAX_RECOVERY_RECORDS {
                return Err(AppError::invalid("更新恢复记录数量超过允许上限"));
            }
            let directory = entry.path();
            let receipt = read_latest_receipt(&directory)?;
            validate_receipt(&receipt, id)?;
            let artifact_path = directory.join(ARTIFACT_FILE);
            let artifact_exists = match fs::symlink_metadata(&artifact_path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(AppError::invalid("更新恢复包必须是普通文件"));
                }
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(AppError::infrastructure_with_source(
                        "检查更新恢复包失败",
                        error,
                    ));
                }
            };
            if artifact_exists {
                let digest = hash_bounded_file(&artifact_path, self.max_artifact_bytes).map_err(
                    |error| AppError::infrastructure_with_source("校验更新恢复包失败", error),
                )?;
                if digest != receipt.artifact_sha256 {
                    return Err(AppError::invalid("更新恢复包摘要与收据不匹配"));
                }
            } else if receipt.state != ReceiptState::AwaitingConfirmation {
                return Err(AppError::invalid("更新恢复收据引用的安装包缺失"));
            }

            let version_matches =
                Version::parse(&receipt.version).is_ok_and(|version| version == current_version);
            let target_matches = receipt.target == current_target;
            if receipt.state == ReceiptState::AwaitingConfirmation
                && version_matches
                && target_matches
                && receipt.installed_target_sha256.as_deref() == current_target_sha256
                && receipt.installed_target_sha256.is_some()
            {
                self.remove(id)?;
                continue;
            }

            let state = match receipt.state {
                ReceiptState::PendingConfirmation => UpdateRecoveryState::PendingConfirmation,
                ReceiptState::Installing if version_matches && target_matches => {
                    UpdateRecoveryState::VersionMatchedAwaitingConfirmation
                }
                ReceiptState::Installing => UpdateRecoveryState::OutcomeUnknown,
                ReceiptState::AwaitingConfirmation if !version_matches || !target_matches => {
                    UpdateRecoveryState::PendingConfirmation
                }
                ReceiptState::AwaitingConfirmation
                    if receipt.installed_target_sha256.is_none()
                        && version_matches
                        && target_matches =>
                {
                    UpdateRecoveryState::VersionMatchedAwaitingConfirmation
                }
                ReceiptState::AwaitingConfirmation => UpdateRecoveryState::OutcomeUnknown,
            };
            records.push(UpdateRecoveryRecord {
                id,
                version: receipt.version,
                target: receipt.target,
                artifact_sha256: receipt.artifact_sha256,
                state,
                recovery_path: directory,
            });
        }
        Ok(records)
    }

    /// 仅由用户显式确认或安全的安装前放弃路径调用，绝不跟随目录符号链接删除。
    pub(super) fn remove(&self, id: UpdateRecoveryId) -> AppResult<()> {
        let directory = self.record_dir(id);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                Err(AppError::invalid("更新恢复记录路径不安全"))
            }
            Ok(_) => {
                fs::remove_dir_all(&directory).map_err(|error| {
                    AppError::infrastructure_with_source("清理已确认的更新恢复材料失败", error)
                })?;
                sync_directory(&self.root).map_err(|error| {
                    AppError::infrastructure_with_source("同步更新恢复清理结果失败", error)
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::infrastructure_with_source(
                "读取更新恢复记录失败",
                error,
            )),
        }
    }

    /// 返回受恢复根约束的普通 artifact 路径，阻止 SDK 接收任意路径或符号链接。
    pub(super) fn artifact_path(&self, id: UpdateRecoveryId) -> AppResult<PathBuf> {
        let directory = self.checked_record_dir(id)?;
        let path = directory.join(ARTIFACT_FILE);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| AppError::infrastructure_with_source("读取更新恢复包失败", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::invalid("更新恢复包路径不安全"));
        }
        Ok(path)
    }

    /// 安装前按收据摘要重读有界 artifact，避免暂存文件被替换后绕过原始验签结果。
    pub(super) fn read_verified_artifact(&self, id: UpdateRecoveryId) -> AppResult<Vec<u8>> {
        let directory = self.checked_record_dir(id)?;
        let receipt = read_latest_receipt(&directory)?;
        validate_receipt(&receipt, id)?;
        if receipt.state != ReceiptState::PendingConfirmation {
            return Err(AppError::conflict("更新恢复记录不处于可安装阶段"));
        }
        let path = self.artifact_path(id)?;
        let bytes = read_bounded_file(&path, self.max_artifact_bytes)
            .map_err(|error| AppError::infrastructure_with_source("读取已验签更新包失败", error))?;
        let artifact_digest: [u8; 32] = Sha256::digest(&bytes).into();
        if hex_digest(&artifact_digest) != receipt.artifact_sha256 {
            return Err(AppError::invalid("已验签更新包摘要与收据不匹配"));
        }
        Ok(bytes)
    }

    /// 只对当前运行目标的普通文件求摘要，供启动时回读 SDK 更新的目标文件。
    pub(super) fn target_sha256(&self, path: &Path) -> AppResult<String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| AppError::infrastructure_with_source("读取当前更新目标失败", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::invalid("当前更新目标必须是普通文件"));
        }
        hash_bounded_file(path, self.max_artifact_bytes)
            .map_err(|error| AppError::infrastructure_with_source("计算更新目标摘要失败", error))
    }

    /// 检查指定记录目录确为恢复根的直接普通子目录，防止路径拼接越界。
    fn checked_record_dir(&self, id: UpdateRecoveryId) -> AppResult<PathBuf> {
        let directory = self.record_dir(id);
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|error| AppError::infrastructure_with_source("读取更新恢复记录失败", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::invalid("更新恢复记录路径不安全"));
        }
        Ok(directory)
    }

    /// 根据不透明 UUID 构造唯一一级子目录，调用方不能传入任意路径片段。
    fn record_dir(&self, id: UpdateRecoveryId) -> PathBuf {
        self.root.join(id.as_uuid().to_string())
    }

    /// 在不覆盖旧阶段文件的前提下追加状态收据，崩溃时总能读取最后一个完整阶段。
    fn transition_receipt(
        &self,
        id: UpdateRecoveryId,
        state: ReceiptState,
        installed_target_sha256: Option<String>,
    ) -> AppResult<()> {
        let directory = self.checked_record_dir(id)?;
        let mut receipt = read_latest_receipt(&directory)?;
        validate_receipt(&receipt, id)?;
        receipt.state = state;
        receipt.installed_target_sha256 = installed_target_sha256;
        let destination = match state {
            ReceiptState::PendingConfirmation => directory.join(PENDING_RECEIPT),
            ReceiptState::Installing => directory.join(INSTALLING_RECEIPT),
            ReceiptState::AwaitingConfirmation => directory.join(AWAITING_RECEIPT),
        };
        if destination.exists() {
            let committed = read_receipt_file(&destination)?;
            validate_receipt(&committed, id)?;
            if committed.state != receipt.state
                || committed.version != receipt.version
                || committed.target != receipt.target
                || committed.artifact_sha256 != receipt.artifact_sha256
                || committed.installed_target_sha256 != receipt.installed_target_sha256
            {
                return Err(AppError::conflict("更新恢复收据状态不可重复提交"));
            }
        } else {
            let temp_path = directory.join(format!("receipt.next-{}.tmp", uuid::Uuid::new_v4()));
            write_new_receipt(&temp_path, &receipt).map_err(|error| {
                AppError::infrastructure_with_source("写入更新恢复收据失败", error)
            })?;
            commit_staged_file(&temp_path, &destination, CommitMode::CreateNew).map_err(
                |error| {
                    map_recovery_commit_error(
                        error,
                        "提交更新恢复状态失败",
                        "提交更新恢复状态结果未知",
                    )
                },
            )?;
        }
        let previous_paths = match state {
            ReceiptState::PendingConfirmation => [Some(directory.join(INSTALLING_RECEIPT)), None],
            ReceiptState::Installing => [Some(directory.join(PENDING_RECEIPT)), None],
            ReceiptState::AwaitingConfirmation => [
                Some(directory.join(INSTALLING_RECEIPT)),
                Some(directory.join(PENDING_RECEIPT)),
            ],
        };
        for previous_path in previous_paths.into_iter().flatten() {
            match fs::remove_file(previous_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(AppError::infrastructure_with_source(
                        "清理旧更新收据失败",
                        error,
                    ));
                }
            }
        }
        sync_directory(&directory)
            .map_err(|error| AppError::infrastructure_with_source("同步更新恢复状态失败", error))?;
        Ok(())
    }
}

/// 限制摘要读取的文件体积，避免恢复目录被替换成超大文件后耗尽磁盘或内存。
fn hash_bounded_file(path: &Path, max_bytes: u64) -> io::Result<String> {
    let file = File::open(path)?;
    if file.metadata()?.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update artifact exceeds limit",
        ));
    }
    let mut reader = io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut received_total = 0_u64;
    loop {
        let received = reader.read(&mut buffer)?;
        if received == 0 {
            break;
        }
        received_total = received_total.checked_add(received as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "update artifact size overflow")
        })?;
        if received_total > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "update artifact exceeds limit",
            ));
        }
        hasher.update(&buffer[..received]);
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

/// 在内存中读取前先核对文件长度，并在读取过程中再次执行硬上限。
fn read_bounded_file(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update artifact exceeds limit",
        ));
    }
    let capacity = usize::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "update artifact is too large"))?;
    let mut reader = io::BufReader::new(file);
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let received = reader.read(&mut buffer)?;
        if received == 0 {
            break;
        }
        let next_len = bytes.len().checked_add(received).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "update artifact size overflow")
        })?;
        if next_len as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "update artifact exceeds limit",
            ));
        }
        bytes.extend_from_slice(&buffer[..received]);
    }
    Ok(bytes)
}

/// 限制 JSON 序列化尺寸并以 create_new 防止覆盖一份已提交状态。
fn write_new_receipt(path: &Path, receipt: &RecoveryReceipt) -> io::Result<()> {
    let bytes = serde_json::to_vec(receipt)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update receipt exceeds limit",
        ));
    }
    let mut file = create_private_file(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

/// 原子移动已同步的恢复材料；提交后的目录同步失败必须保留未知结果分类供调用方回读。
fn map_recovery_commit_error(
    error: AtomicCommitError,
    not_committed_context: &'static str,
    unknown_context: &'static str,
) -> AppError {
    match error {
        AtomicCommitError::NotCommitted(source) => {
            AppError::infrastructure_with_source(not_committed_context, source)
        }
        AtomicCommitError::CommitUnknown(source) => {
            AppError::outcome_unknown_with_source(unknown_context, source)
        }
    }
}

/// 新建仅当前用户可读写的普通文件，避免凭据更新包被同机其他账户读取。
fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// 按最新阶段文件读取收据，多个阶段短暂并存时选择提交顺序最高的一份。
fn read_latest_receipt(directory: &Path) -> AppResult<RecoveryReceipt> {
    for name in [AWAITING_RECEIPT, INSTALLING_RECEIPT, PENDING_RECEIPT] {
        let path = directory.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(AppError::infrastructure_with_source(
                    "读取更新恢复收据失败",
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_RECEIPT_BYTES
        {
            return Err(AppError::invalid("更新恢复收据格式或大小无效"));
        }
        let receipt = read_receipt_file(&path)?;
        let expected_state = match name {
            AWAITING_RECEIPT => ReceiptState::AwaitingConfirmation,
            INSTALLING_RECEIPT => ReceiptState::Installing,
            _ => ReceiptState::PendingConfirmation,
        };
        if receipt.state != expected_state {
            return Err(AppError::invalid("更新恢复阶段与收据文件不匹配"));
        }
        return Ok(receipt);
    }
    Err(AppError::not_found("更新恢复收据不存在"))
}

/// 只读取有界普通收据文件，阶段校验由调用方结合文件名完成。
fn read_receipt_file(path: &Path) -> AppResult<RecoveryReceipt> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::infrastructure_with_source("读取更新恢复收据失败", error))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_RECEIPT_BYTES
    {
        return Err(AppError::invalid("更新恢复收据格式或大小无效"));
    }
    let bytes = fs::read(path)
        .map_err(|error| AppError::infrastructure_with_source("读取更新恢复收据失败", error))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AppError::infrastructure_with_source("更新恢复收据损坏", error))
}

/// 创建阶段文件的持久化边界同时刷新目录元数据，避免重启后丢失已确认状态。
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// 检查 receipt 中的版本、target 与摘要长度，拒绝控制字符和超长 UI 内容。
fn validate_receipt_values(version: &str, target: &str) -> AppResult<()> {
    if Version::parse(version).is_err()
        || version.len() > 128
        || target.is_empty()
        || target.len() > 128
        || target.chars().any(char::is_control)
    {
        return Err(AppError::invalid("更新恢复收据字段无效"));
    }
    Ok(())
}

/// 校验 receipt 身份与所有不可信文本字段，避免目录名和正文身份发生错配。
fn validate_receipt(receipt: &RecoveryReceipt, id: UpdateRecoveryId) -> AppResult<()> {
    if receipt.schema != 1 || receipt.id != id.as_uuid().to_string() {
        return Err(AppError::invalid("更新恢复收据版本或身份不匹配"));
    }
    validate_receipt_values(&receipt.version, &receipt.target)?;
    validate_digest(&receipt.artifact_sha256)?;
    if receipt.state != ReceiptState::AwaitingConfirmation
        && receipt.installed_target_sha256.is_some()
    {
        return Err(AppError::invalid("更新恢复阶段不能包含已安装目标摘要"));
    }
    if let Some(digest) = receipt.installed_target_sha256.as_deref() {
        validate_digest(digest)?;
    }
    Ok(())
}

/// 只接受 32-byte SHA-256 的小写十六进制表示，避免摘要显示和比较有多种编码。
fn validate_digest(digest: &str) -> AppResult<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::invalid("更新恢复摘要格式无效"));
    }
    Ok(())
}

/// 仅从一级目录名解析 UUID，所有其他文件名和 staging 目录都不作为恢复记录。
fn parse_record_id(name: &str) -> Option<UpdateRecoveryId> {
    uuid::Uuid::parse_str(name)
        .ok()
        .map(UpdateRecoveryId::from_uuid)
}

/// 将 SHA-256 摘要格式化为唯一小写 hex 形式供 receipt 与 UI 使用。
fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
#[path = "../../../tests/support/updater_recovery_unit.rs"]
mod tests;
