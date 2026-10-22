//! schema 1 vault key 的读取、导入、备份与轮换。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf, str};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::credentials::{
        VaultKeyAvailability, VaultKeyDocument, VaultKeyExportMode, VaultKeyOperationReceipt,
        VaultKeyRepository, VaultRecoveryReason,
    },
};
use tokio::task;
use zeroize::Zeroizing;

use super::{
    VaultStore,
    error::{VaultError, to_app_error},
    format::{self, VaultFile, VaultKeyFile},
    store::{parse_key_bytes, parse_vault_bytes},
    transaction::{
        self, PairUpdate, create_legacy_backup, export_key_file, key_path, read_pair,
        read_pair_readonly, vault_path,
    },
};

const MAX_KEY_DOCUMENT_BYTES: u64 = 128 * 1024;
const MAX_VAULT_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

impl VaultStore {
    /// 恢复本地 pair journal 后诊断文件；损坏时返回稳定原因码，不创建默认 key 或替换文件。
    pub(super) fn availability_sync(&self) -> VaultKeyAvailability {
        let _guard = match self.storage.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return VaultKeyAvailability::RecoveryRequired {
                    reason: VaultRecoveryReason::StorageUnavailable,
                };
            }
        };
        if let Err(error) = transaction::recover_pending(&self.storage) {
            return VaultKeyAvailability::RecoveryRequired {
                reason: recovery_reason(&error),
            };
        }
        let (key_source, vault_source) = match read_pair_readonly(&self.storage) {
            Ok(pair) => pair,
            Err(error) => {
                return VaultKeyAvailability::RecoveryRequired {
                    reason: recovery_reason(&error),
                };
            }
        };
        let key = match key_source.as_ref() {
            Some(source) => match parse_key_bytes(source.as_slice()) {
                Ok(key) => Some(key),
                Err(_) => {
                    return VaultKeyAvailability::RecoveryRequired {
                        reason: VaultRecoveryReason::InvalidKey,
                    };
                }
            },
            None if vault_source.is_some() => {
                if parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice())).is_err()
                {
                    return VaultKeyAvailability::RecoveryRequired {
                        reason: VaultRecoveryReason::InvalidVault,
                    };
                }
                return VaultKeyAvailability::MissingForExistingVault;
            }
            None => return VaultKeyAvailability::NotInitialized,
        };
        let vault = match parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice())) {
            Ok(vault) => vault,
            Err(_) => {
                return VaultKeyAvailability::RecoveryRequired {
                    reason: VaultRecoveryReason::InvalidVault,
                };
            }
        };
        let Some(key) = key else {
            return VaultKeyAvailability::NotInitialized;
        };
        if validate_key_against_vault(&key, &vault).is_err() {
            return VaultKeyAvailability::RecoveryRequired {
                reason: VaultRecoveryReason::KeyMismatch,
            };
        }
        VaultKeyAvailability::Ready { key_id: key.key_id }
    }

    /// 仅在完全空白工作区自动生成 key；密文存在而 key 缺失时进入恢复状态。
    pub(super) fn ensure_key_sync(&self) -> Result<VaultKeyAvailability, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        if let Err(error) = transaction::recover_pending(&self.storage) {
            return Ok(VaultKeyAvailability::RecoveryRequired {
                reason: recovery_reason(&error),
            });
        }
        let (key_source, vault_source) = match read_pair(&self.storage) {
            Ok(pair) => pair,
            Err(error) => {
                return Ok(VaultKeyAvailability::RecoveryRequired {
                    reason: recovery_reason(&error),
                });
            }
        };
        if let Some(source) = key_source.as_ref() {
            let key = match parse_key_bytes(source.as_slice()) {
                Ok(key) => key,
                Err(_) => {
                    return Ok(VaultKeyAvailability::RecoveryRequired {
                        reason: VaultRecoveryReason::InvalidKey,
                    });
                }
            };
            let vault =
                match parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice())) {
                    Ok(vault) => vault,
                    Err(_) => {
                        return Ok(VaultKeyAvailability::RecoveryRequired {
                            reason: VaultRecoveryReason::InvalidVault,
                        });
                    }
                };
            if validate_key_against_vault(&key, &vault).is_err() {
                return Ok(VaultKeyAvailability::RecoveryRequired {
                    reason: VaultRecoveryReason::KeyMismatch,
                });
            }
            return Ok(VaultKeyAvailability::Ready { key_id: key.key_id });
        }
        if vault_source.is_some() {
            if parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice())).is_err() {
                return Ok(VaultKeyAvailability::RecoveryRequired {
                    reason: VaultRecoveryReason::InvalidVault,
                });
            }
            return Ok(VaultKeyAvailability::MissingForExistingVault);
        }
        let key = format::new_key()?;
        let key_id = key.key_id.clone();
        let source = format::encode_key(&key)?;
        transaction::commit_pair_updates(
            &self.storage,
            PairUpdate {
                key: Some(source.as_bytes()),
                vault: None,
            },
        )?;
        Ok(VaultKeyAvailability::Created { key_id })
    }

    /// 只在 key 与 vault 均不存在时显式创建首个 workspace key。
    pub(super) fn create_key_sync(&self) -> Result<Option<VaultKeyOperationReceipt>, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (key_source, vault_source) = read_pair(&self.storage)?;
        if key_source.is_some() || vault_source.is_some() {
            return Ok(None);
        }
        let key = format::new_key()?;
        let key_id = key.key_id.clone();
        let source = format::encode_key(&key)?;
        transaction::commit_pair_updates(
            &self.storage,
            PairUpdate {
                key: Some(source.as_bytes()),
                vault: None,
            },
        )?;
        Ok(Some(VaultKeyOperationReceipt {
            key_id,
            dry_run: false,
            entry_count: 0,
            backup_created: false,
        }))
    }

    /// 读取并校验当前 key 的原始 TOML，便于兼容旧版用户离线备份。
    pub(super) fn read_key_sync(&self) -> Result<VaultKeyDocument, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (source, vault_source) = read_pair(&self.storage)?;
        let bytes = source.ok_or(VaultError::MissingKey)?;
        let text = str::from_utf8(bytes.as_slice()).map_err(|_| VaultError::InvalidData)?;
        let key = format::parse_key(text)?;
        let vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        validate_key_against_vault(&key, &vault)?;
        Ok(VaultKeyDocument::from_zeroizing(Zeroizing::new(
            text.to_owned(),
        )))
    }

    /// 按旧版 export_key_toml 行为输出 schema 1 pretty TOML。
    pub(super) fn export_key_sync(&self) -> Result<VaultKeyDocument, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (source, vault_source) = read_pair(&self.storage)?;
        let bytes = source.ok_or(VaultError::MissingKey)?;
        let text = str::from_utf8(bytes.as_slice()).map_err(|_| VaultError::InvalidData)?;
        let key = format::parse_key(text)?;
        let vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        validate_key_against_vault(&key, &vault)?;
        Ok(VaultKeyDocument::from_zeroizing(format::encode_key(&key)?))
    }

    /// 导出时在同一 coordinator 锁内验证当前 pair，避免把轮换前的过期 key 误作当前备份。
    pub(super) fn export_key_to_path_sync(
        &self,
        path: PathBuf,
        mode: VaultKeyExportMode,
    ) -> Result<VaultKeyOperationReceipt, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (key_source, vault_source) = read_pair(&self.storage)?;
        let bytes = key_source.as_ref().ok_or(VaultError::MissingKey)?;
        let key = parse_key_bytes(bytes.as_slice())?;
        let vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        validate_key_against_vault(&key, &vault)?;
        let document = format::encode_key(&key)?;
        export_key_file(&path, document.as_bytes(), mode)?;
        Ok(VaultKeyOperationReceipt {
            key_id: key.key_id,
            dry_run: false,
            entry_count: vault.entries.len(),
            backup_created: false,
        })
    }

    /// 校验输入 key 能解密所有旧 entries，再备份旧 key 并用 pair journal 原子替换。
    pub(super) fn import_key_sync(
        &self,
        source: Zeroizing<String>,
        dry_run: bool,
    ) -> Result<VaultKeyOperationReceipt, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let next_key = format::parse_key(source.as_str())?;
        let (old_key_source, vault_source) = read_pair(&self.storage)?;
        let vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        validate_key_against_vault(&next_key, &vault)?;
        let backup_created = !dry_run && old_key_source.is_some();
        if !dry_run {
            if let Some(bytes) = old_key_source.as_ref() {
                create_legacy_backup(&self.storage, &key_path(&self.storage), bytes.as_slice())?;
            }
            let normalized = format::encode_key(&next_key)?;
            transaction::commit_pair_updates(
                &self.storage,
                PairUpdate {
                    key: Some(normalized.as_bytes()),
                    vault: None,
                },
            )?;
        }
        Ok(VaultKeyOperationReceipt {
            key_id: next_key.key_id,
            dry_run,
            entry_count: vault.entries.len(),
            backup_created,
        })
    }

    /// 从用户选定的两个文件恢复完整 pair；先全量解密校验，再备份旧损坏内容并原子提交。
    pub(super) fn restore_pair_from_paths_sync(
        &self,
        key_path: PathBuf,
        vault_path: PathBuf,
        dry_run: bool,
    ) -> Result<VaultKeyOperationReceipt, VaultError> {
        let key_source = read_selected_file(&key_path, MAX_KEY_DOCUMENT_BYTES)?;
        let vault_source = read_selected_file(&vault_path, MAX_VAULT_DOCUMENT_BYTES)?;
        let key_text =
            str::from_utf8(key_source.as_slice()).map_err(|_| VaultError::InvalidData)?;
        let vault_text =
            str::from_utf8(vault_source.as_slice()).map_err(|_| VaultError::InvalidData)?;
        let key = format::parse_key(key_text)?;
        let vault = format::parse_vault(vault_text)?;
        validate_key_against_vault(&key, &vault)?;

        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (old_key, old_vault) = read_pair(&self.storage)?;
        let backup_created = !dry_run && (old_key.is_some() || old_vault.is_some());
        if !dry_run {
            if let Some(bytes) = old_key.as_ref() {
                create_legacy_backup(
                    &self.storage,
                    &super::transaction::key_path(&self.storage),
                    bytes.as_slice(),
                )?;
            }
            if let Some(bytes) = old_vault.as_ref() {
                create_legacy_backup(
                    &self.storage,
                    &super::transaction::vault_path(&self.storage),
                    bytes.as_slice(),
                )?;
            }
            transaction::commit_pair_updates(
                &self.storage,
                PairUpdate {
                    key: Some(key_source.as_slice()),
                    vault: Some(vault_source.as_slice()),
                },
            )?;
        }
        Ok(VaultKeyOperationReceipt {
            key_id: key.key_id,
            dry_run,
            entry_count: vault.entries.len(),
            backup_created,
        })
    }

    /// 生成新 key 并重加密所有 entry；旧 pair 先备份，提交由 recovery journal 保护。
    pub(super) fn rotate_key_sync(
        &self,
        dry_run: bool,
    ) -> Result<VaultKeyOperationReceipt, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (old_key_source, vault_source) = read_pair(&self.storage)?;
        let old_key = parse_key_bytes(
            old_key_source
                .as_ref()
                .map(|source| source.as_slice())
                .ok_or(VaultError::MissingKey)?,
        )?;
        let mut vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        let new_key = format::new_key()?;
        let mut next_entries = Vec::with_capacity(vault.entries.len());
        for previous in &vault.entries {
            let aad = format::associated_data(previous)?;
            let plaintext = format::decrypt(&old_key, previous, &aad)?;
            let mut next = format::encrypt(
                &new_key,
                &previous.id,
                &previous.kind,
                &aad,
                plaintext.as_slice(),
            )?;
            next.created_at.clone_from(&previous.created_at);
            next_entries.push(next);
        }
        let entry_count = vault.entries.len();
        let key_id = new_key.key_id.clone();
        if !dry_run {
            if let Some(bytes) = old_key_source.as_ref() {
                create_legacy_backup(&self.storage, &key_path(&self.storage), bytes.as_slice())?;
            }
            if let Some(bytes) = vault_source.as_ref() {
                create_legacy_backup(&self.storage, &vault_path(&self.storage), bytes.as_slice())?;
            }
            vault.entries = next_entries;
            let key_text = format::encode_key(&new_key)?;
            let vault_text = format::encode_vault(&vault)?;
            transaction::commit_pair_updates(
                &self.storage,
                PairUpdate {
                    key: Some(key_text.as_bytes()),
                    vault: Some(vault_text.as_bytes()),
                },
            )?;
        }
        Ok(VaultKeyOperationReceipt {
            key_id,
            dry_run,
            entry_count,
            backup_created: !dry_run,
        })
    }
}

#[async_trait]
impl VaultKeyRepository for VaultStore {
    /// 修复可判定的本地 journal 后返回 typed 状态；损坏密文不会使整个桌面启动失败。
    async fn availability(&self) -> AppResult<VaultKeyAvailability> {
        let store = self.clone();
        task::spawn_blocking(move || store.availability_sync())
            .await
            .map_err(|source| AppError::infrastructure_with_source("检查凭据库状态失败", source))
    }

    /// 将 workspace bootstrap 与恢复检查放入阻塞池并统一取消不可中断状态。
    async fn ensure_key(&self) -> AppResult<VaultKeyAvailability> {
        let store = self.clone();
        task::spawn_blocking(move || store.ensure_key_sync())
            .await
            .map_err(|source| AppError::infrastructure_with_source("初始化凭据库失败", source))?
            .map_err(|source| to_app_error("初始化凭据库失败", source))
    }

    /// 显式首次创建不允许覆盖任何现有配置或密文。
    async fn create_key(&self) -> AppResult<VaultKeyOperationReceipt> {
        let store = self.clone();
        let receipt = task::spawn_blocking(move || store.create_key_sync())
            .await
            .map_err(|source| AppError::infrastructure_with_source("创建凭据库 key 失败", source))?
            .map_err(|source| to_app_error("创建凭据库 key 失败", source))?;
        receipt.ok_or_else(|| AppError::conflict("凭据库 key 或密文已存在"))
    }

    /// 读取原始 key 文档；密文缺 key 时错误只显示静态恢复提示。
    async fn read_key(&self) -> AppResult<VaultKeyDocument> {
        let store = self.clone();
        task::spawn_blocking(move || store.read_key_sync())
            .await
            .map_err(|source| AppError::infrastructure_with_source("读取凭据库 key 失败", source))?
            .map_err(|source| to_app_error("读取凭据库 key 失败", source))
    }

    /// 导出格式沿用 schema 1，key TOML 只存在于零化返回值中。
    async fn export_key(&self) -> AppResult<VaultKeyDocument> {
        let store = self.clone();
        task::spawn_blocking(move || store.export_key_sync())
            .await
            .map_err(|source| AppError::infrastructure_with_source("导出凭据库 key 失败", source))?
            .map_err(|source| to_app_error("导出凭据库 key 失败", source))
    }

    /// 输出安全备份文件时使用明确的新建/覆盖模式与同目录原子替换。
    async fn export_key_to_path(
        &self,
        path: PathBuf,
        mode: VaultKeyExportMode,
    ) -> AppResult<VaultKeyOperationReceipt> {
        let store = self.clone();
        task::spawn_blocking(move || store.export_key_to_path_sync(path, mode))
            .await
            .map_err(|source| AppError::infrastructure_with_source("导出凭据库 key 失败", source))?
            .map_err(|source| to_app_error("导出凭据库 key 失败", source))
    }

    /// Save 是旧版 import_key(dry_run=false) 的 typed 别名。
    async fn save_key(&self, document: VaultKeyDocument) -> AppResult<VaultKeyOperationReceipt> {
        self.import_key(document, false).await
    }

    /// 导入仅在输入 key 可解密全部 entries 时提交，未知或不匹配不覆盖现有文件。
    async fn import_key(
        &self,
        document: VaultKeyDocument,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        let store = self.clone();
        task::spawn_blocking(move || store.import_key_sync(document.into_zeroizing(), dry_run))
            .await
            .map_err(|source| AppError::infrastructure_with_source("导入凭据库 key 失败", source))?
            .map_err(|source| to_app_error("导入凭据库 key 失败", source))
    }

    /// 在阻塞池读取选定 key 文件并立刻零化缓冲，再复用严格验证的导入用例。
    async fn import_key_from_path(
        &self,
        path: PathBuf,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        let document = task::spawn_blocking(move || read_key_document(&path))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("读取待导入凭据库 key 失败", source)
            })?
            .map_err(|source| to_app_error("读取待导入凭据库 key 失败", source))?;
        self.import_key(document, dry_run).await
    }

    /// 从两个用户选定文件恢复 pair；未知提交状态保留 transaction journal 与备份供下次诊断。
    async fn restore_pair_from_paths(
        &self,
        key_path: PathBuf,
        vault_path: PathBuf,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        let store = self.clone();
        task::spawn_blocking(move || {
            store.restore_pair_from_paths_sync(key_path, vault_path, dry_run)
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("恢复凭据库文件失败", source))?
        .map_err(|source| to_app_error("恢复凭据库文件失败", source))
    }

    /// 轮换失败不清理既有备份，且对不可解密 entries fail closed。
    async fn rotate_key(&self, dry_run: bool) -> AppResult<VaultKeyOperationReceipt> {
        let store = self.clone();
        task::spawn_blocking(move || store.rotate_key_sync(dry_run))
            .await
            .map_err(|source| AppError::infrastructure_with_source("轮换凭据库 key 失败", source))?
            .map_err(|source| to_app_error("轮换凭据库 key 失败", source))
    }
}

/// 用当前 key 验证整个 vault，防止把无法解密的文件 pair 写成持久化状态。
pub(super) fn validate_key_against_vault(
    key: &VaultKeyFile,
    vault: &VaultFile,
) -> Result<(), VaultError> {
    for entry in &vault.entries {
        let aad = format::associated_data(entry)?;
        format::decrypt(key, entry, &aad)?;
    }
    Ok(())
}

/// 将 key 文件导入路径转换为脱敏文档，拒绝链接、非普通文件与超限输入。
fn read_key_document(path: &std::path::Path) -> Result<VaultKeyDocument, VaultError> {
    let source = read_selected_file(path, MAX_KEY_DOCUMENT_BYTES)?;
    let text = str::from_utf8(source.as_slice()).map_err(|_| VaultError::InvalidData)?;
    format::parse_key(text)?;
    Ok(VaultKeyDocument::from_zeroizing(Zeroizing::new(
        text.to_owned(),
    )))
}

/// 读取用户明确选择的普通文件，并在返回前将原始字节放入 zeroize 容器。
fn read_selected_file(
    path: &std::path::Path,
    max_bytes: u64,
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes {
        return Err(
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                VaultError::UnsafePath
            } else {
                VaultError::InvalidData
            },
        );
    }
    let source = Zeroizing::new(fs::read(path)?);
    if source.len() as u64 > max_bytes {
        return Err(VaultError::InvalidData);
    }
    Ok(source)
}

/// 恢复诊断只记录有限状态原因，不从底层 IO 文案或 TOML 错误提取用户内容。
fn recovery_reason(error: &VaultError) -> VaultRecoveryReason {
    match error {
        VaultError::InvalidData => VaultRecoveryReason::PairTransaction,
        VaultError::RecoveryRequired
        | VaultError::CommitNotCommitted(_)
        | VaultError::CommitOutcomeUnknown(_) => VaultRecoveryReason::PairTransaction,
        VaultError::UnsafePath => VaultRecoveryReason::UnsafePath,
        VaultError::KeyMismatch => VaultRecoveryReason::KeyMismatch,
        VaultError::MissingKey => VaultRecoveryReason::StorageUnavailable,
        VaultError::Io(_) | VaultError::LockUnavailable => VaultRecoveryReason::StorageUnavailable,
        VaultError::MissingEntry
        | VaultError::ExportTargetExists
        | VaultError::ExportTargetMissing => VaultRecoveryReason::StorageUnavailable,
    }
}
