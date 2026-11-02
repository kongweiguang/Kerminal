//! 本地主机环境引用的跨进程恢复日志。
//!
//! @author kongweiguang

use std::{fs, fs::OpenOptions, io, path::Path};

use kerminal_application::{
    AppError, AppResult, connections::management::LocalHostSaveRecoveryToken,
};
use kerminal_domain::{connections::CredentialRef, identity::HostId, preferences::ConfigRevision};
use serde::{Deserialize, Serialize};

use crate::storage::{AtomicCommitError, CommitMode, remove_file_with_parent_sync};

use super::{ConfigurationStore, atomic_replace, final_path_is_symlink};

const RECOVERY_PATH: &str = ".storage-transactions/local-host-environment.json";
const OPERATION_LOCK_PATH: &str = ".storage-transactions/local-host-operation.lock";
const MAX_RECOVERY_BYTES: u64 = 16 * 1024;

/// 日志仅保存不透明引用与 CAS 基线；密文仍由 Vault 自己的事务管理。
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryDocument {
    schema_version: u32,
    operation_id: String,
    host_id: String,
    baseline_revision: Option<String>,
    old_reference: Option<String>,
    new_reference: Option<String>,
}

impl ConfigurationStore {
    /// 独立文件锁覆盖异步事务全程；不持有共享存储锁，避免与 vault 或配置读写死锁。
    ///
    /// @author kongweiguang
    pub(crate) fn acquire_local_host_operation(&self) -> AppResult<fs::File> {
        let _lock = self.inner.storage.lock().map_err(|source| {
            AppError::infrastructure_with_source("无法锁定本地主机操作目录", source)
        })?;
        ensure_recovery_directory(&self.inner.root)?;
        let path = self.inner.root.join(OPERATION_LOCK_PATH);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(AppError::configuration_corrupt("本地主机操作锁无效"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "检查本地主机操作锁失败",
                    source,
                ));
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| {
                AppError::infrastructure_with_source("打开本地主机操作锁失败", source)
            })?;
        if !file
            .metadata()
            .map_err(|source| {
                AppError::infrastructure_with_source("检查本地主机操作锁失败", source)
            })?
            .is_file()
            || final_path_is_symlink(&path).map_err(|source| {
                AppError::infrastructure_with_source("检查本地主机操作锁失败", source)
            })?
        {
            return Err(AppError::configuration_corrupt("本地主机操作锁无效"));
        }
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => {
                Err(AppError::conflict("本地主机操作正在进行，请稍后重试"))
            }
            Err(fs::TryLockError::Error(source)) => Err(AppError::infrastructure_with_source(
                "获取本地主机操作锁失败",
                source,
            )),
        }
    }

    /// 先登记唯一操作，进程退出后仍可从 host 当前引用确定密文去留。
    ///
    /// @author kongweiguang
    pub(crate) fn begin_local_host_recovery(
        &self,
        token: &LocalHostSaveRecoveryToken,
    ) -> AppResult<()> {
        self.ensure_writable()?;
        let _lock = self.inner.storage.lock().map_err(|source| {
            AppError::infrastructure_with_source("无法锁定本地主机恢复记录", source)
        })?;
        self.ensure_writable()?;
        self.ensure_configuration_transactions_absent()?;
        ensure_recovery_directory(&self.inner.root)?;
        let document = RecoveryDocument {
            schema_version: 1,
            operation_id: token.operation_id().to_owned(),
            host_id: token.host_id().as_str().to_owned(),
            baseline_revision: token
                .baseline_revision()
                .map(|revision| revision.as_str().to_owned()),
            old_reference: token
                .old_reference()
                .map(|reference| reference.as_str().to_owned()),
            new_reference: token
                .new_reference()
                .map(|reference| reference.as_str().to_owned()),
        };
        let bytes = serde_json::to_vec(&document)
            .map_err(|_| AppError::infrastructure("无法编码本地主机恢复记录"))?;
        if bytes.len() as u64 > MAX_RECOVERY_BYTES {
            return Err(AppError::invalid("本地主机恢复记录超过限制"));
        }
        let path = self.inner.root.join(RECOVERY_PATH);
        atomic_replace(&self.inner.root, Path::new(RECOVERY_PATH), &bytes, || {
            match fs::symlink_metadata(&path) {
                Ok(_) => Err(AppError::conflict("上次本地主机操作需要先恢复")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CommitMode::CreateNew),
                Err(source) => Err(AppError::infrastructure_with_source(
                    "检查本地主机恢复记录失败",
                    source,
                )),
            }
        })
    }

    /// 只读取有界、非链接日志并重新验证引用归属，损坏记录不能触发任意凭据删除。
    ///
    /// @author kongweiguang
    pub(crate) fn pending_local_host_recovery(
        &self,
    ) -> AppResult<Option<LocalHostSaveRecoveryToken>> {
        let _lock = self.inner.storage.lock().map_err(|source| {
            AppError::infrastructure_with_source("无法锁定本地主机恢复记录", source)
        })?;
        read_recovery_document(&self.inner.root)
    }

    /// 仅删除与已完成操作完全一致的日志，重复清理缺失记录保持幂等。
    ///
    /// @author kongweiguang
    pub(crate) fn finish_local_host_recovery(
        &self,
        token: &LocalHostSaveRecoveryToken,
    ) -> AppResult<()> {
        let _lock = self.inner.storage.lock().map_err(|source| {
            AppError::infrastructure_with_source("无法锁定本地主机恢复记录", source)
        })?;
        match read_recovery_document(&self.inner.root)? {
            None => return Ok(()),
            Some(current) if &current != token => {
                return Err(AppError::conflict("本地主机恢复记录已变化"));
            }
            Some(_) => {}
        }
        let path = self.inner.root.join(RECOVERY_PATH);
        if final_path_is_symlink(&path).map_err(|source| {
            AppError::infrastructure_with_source("检查本地主机恢复记录失败", source)
        })? {
            return Err(AppError::configuration_corrupt("本地主机恢复记录无效"));
        }
        match remove_file_with_parent_sync(&path) {
            Ok(()) => Ok(()),
            Err(AtomicCommitError::NotCommitted(source)) => Err(
                AppError::infrastructure_with_source("清理本地主机恢复记录失败", source),
            ),
            Err(AtomicCommitError::CommitUnknown(source)) => Err(
                AppError::outcome_unknown_with_source("本地主机恢复记录清理结果未知", source),
            ),
        }
    }
}

/// 保持恢复目录为应用拥有的普通目录，拒绝符号链接并限制 Unix 访问权限。
///
/// @author kongweiguang
fn ensure_recovery_directory(root: &Path) -> AppResult<()> {
    let directory = root.join(".storage-transactions");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::configuration_corrupt("配置事务目录无效"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&directory).map_err(|source| {
                AppError::infrastructure_with_source("创建配置事务目录失败", source)
            })?;
        }
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "检查配置事务目录失败",
                source,
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(|source| {
            AppError::infrastructure_with_source("保护配置事务目录失败", source)
        })?;
    }
    Ok(())
}

/// 解码前校验目录与文件类型，再转换为受限制的恢复令牌。
///
/// @author kongweiguang
fn read_recovery_document(root: &Path) -> AppResult<Option<LocalHostSaveRecoveryToken>> {
    let directory = root.join(".storage-transactions");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::configuration_corrupt("配置事务目录无效"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "检查配置事务目录失败",
                source,
            ));
        }
    }
    let path = root.join(RECOVERY_PATH);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "检查本地主机恢复记录失败",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_RECOVERY_BYTES
    {
        return Err(AppError::configuration_corrupt("本地主机恢复记录无效"));
    }
    let bytes = fs::read(&path).map_err(|source| {
        AppError::infrastructure_with_source("读取本地主机恢复记录失败", source)
    })?;
    if bytes.len() as u64 > MAX_RECOVERY_BYTES {
        return Err(AppError::configuration_corrupt("本地主机恢复记录无效"));
    }
    let document: RecoveryDocument = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::configuration_corrupt("本地主机恢复记录无效"))?;
    if document.schema_version != 1 {
        return Err(AppError::configuration_corrupt("本地主机恢复记录版本无效"));
    }
    let host_id = HostId::parse(&document.host_id)
        .map_err(|_| AppError::configuration_corrupt("本地主机恢复目标无效"))?;
    let old_reference = document
        .old_reference
        .map(CredentialRef::new)
        .transpose()
        .map_err(|_| AppError::configuration_corrupt("本地主机恢复引用无效"))?;
    let new_reference = document
        .new_reference
        .map(CredentialRef::new)
        .transpose()
        .map_err(|_| AppError::configuration_corrupt("本地主机恢复引用无效"))?;
    LocalHostSaveRecoveryToken::from_parts(
        document.operation_id,
        host_id,
        document.baseline_revision.map(ConfigRevision::from_sha256),
        old_reference,
        new_reference,
    )
    .map(Some)
    .map_err(|_| AppError::configuration_corrupt("本地主机恢复记录无效"))
}
