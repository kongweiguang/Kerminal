//! Credential vault repository 与连接 resolver adapter。
//!
//! @author kongweiguang

use std::str;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CredentialResolver, ResolvedCredential,
        credentials::{CredentialVaultRepository, CredentialVaultSaveMetadata},
    },
};
use kerminal_domain::connections::{
    CredentialRef,
    credentials::{CredentialMaterial, CredentialReference},
};
use tokio::task;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::{
    error::{VaultError, to_app_error},
    format::{self, VaultFile, VaultKeyFile},
    transaction::{self, PairUpdate, read_pair},
};

/// 故意限制单条凭据大小，避免损坏调用把无界内容交给加密和文件存储。
const MAX_SECRET_BYTES: usize = 4 * 1024 * 1024;

impl super::VaultStore {
    /// 加密并持久化 secret；首条凭据仅在空工作区生成 key/vault pair。
    pub(super) fn save_sync(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> Result<CredentialVaultSaveMetadata, VaultError> {
        if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
            return Err(VaultError::InvalidData);
        }
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (key_source, vault_source) = read_pair(&self.storage)?;
        let mut key = match key_source.as_ref() {
            Some(source) => Some(parse_key_bytes(source.as_slice())?),
            None if vault_source.is_none() => None,
            None => return Err(VaultError::MissingKey),
        };
        let mut vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        if let Some(key) = key.as_ref() {
            super::key_management::validate_key_against_vault(key, &vault)?;
        }
        let created_key = key.is_none();
        if created_key {
            key = Some(format::new_key()?);
        }
        let key = key.as_ref().ok_or(VaultError::MissingKey)?;
        let reference_text = reference.reference().as_str();
        let mut next_entry = format::encrypt(
            key,
            reference_text,
            reference.kind().as_str(),
            reference_text.as_bytes(),
            secret.as_bytes(),
        )?;
        if let Some(previous) = vault
            .entries
            .iter()
            .find(|entry| entry.id == reference_text)
        {
            next_entry.created_at.clone_from(&previous.created_at);
            vault.entries.retain(|entry| entry.id != reference_text);
        }
        let metadata = CredentialVaultSaveMetadata {
            entry_id: next_entry.id.clone(),
            kind: next_entry.kind.clone(),
            key_id: next_entry.key_id.clone(),
            updated_at: next_entry.updated_at.clone(),
        };
        vault.entries.push(next_entry);
        vault.entries.sort_by(|left, right| left.id.cmp(&right.id));
        let vault_source = format::encode_vault(&vault)?;
        let key_source = if created_key {
            Some(format::encode_key(key)?)
        } else {
            None
        };
        transaction::commit_pair_updates(
            &self.storage,
            PairUpdate {
                key: key_source.as_ref().map(|source| source.as_bytes()),
                vault: Some(vault_source.as_bytes()),
            },
        )?;
        Ok(metadata)
    }

    /// 仅按 vault 条目判断引用是否存在，不要求本机 key；缺 key 时仍要区分已保存密文与未保存。
    /// 实际解密继续由 resolve fail closed。
    ///
    /// @author kongweiguang
    pub(super) fn contains_sync(&self, reference: &CredentialRef) -> Result<bool, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (key_source, vault_source) = read_pair(&self.storage)?;
        let vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        if let Some(source) = key_source.as_ref() {
            let key = parse_key_bytes(source.as_slice())?;
            super::key_management::validate_key_against_vault(&key, &vault)?;
        }
        Ok(vault
            .entries
            .iter()
            .any(|entry| entry.id == reference.as_str()))
    }

    /// 原子移除单条密文；key 文件保留以兼容其它主机或后续新增凭据。
    pub(super) fn remove_sync(&self, reference: &CredentialRef) -> Result<bool, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (key_source, vault_source) = read_pair(&self.storage)?;
        let mut vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        if let Some(source) = key_source.as_ref() {
            let key = parse_key_bytes(source.as_slice())?;
            super::key_management::validate_key_against_vault(&key, &vault)?;
        } else if !vault.entries.is_empty() {
            return Err(VaultError::MissingKey);
        }
        let old_count = vault.entries.len();
        vault.entries.retain(|entry| entry.id != reference.as_str());
        if vault.entries.len() == old_count {
            return Ok(false);
        }
        let source = format::encode_vault(&vault)?;
        transaction::commit_pair_updates(
            &self.storage,
            PairUpdate {
                key: None,
                vault: Some(source.as_bytes()),
            },
        )?;
        Ok(true)
    }

    /// 解密单条凭据并按材料类型转换为受 zeroize 保护的运行时凭据。
    pub(super) fn resolve_sync(
        &self,
        reference: &CredentialRef,
    ) -> Result<ResolvedCredential, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (key_source, vault_source) = read_pair(&self.storage)?;
        let key = parse_key_bytes(
            key_source
                .as_ref()
                .map(|source| source.as_slice())
                .ok_or(VaultError::MissingKey)?,
        )?;
        let vault = parse_vault_bytes(vault_source.as_ref().map(|source| source.as_slice()))?;
        super::key_management::validate_key_against_vault(&key, &vault)?;
        let entry = vault
            .entries
            .iter()
            .find(|entry| entry.id == reference.as_str())
            .ok_or(VaultError::MissingEntry)?;
        let parsed_reference =
            CredentialReference::parse(reference.clone()).map_err(|_| VaultError::InvalidData)?;
        // 旧版本把 `kind` 当描述标签使用（例如 ssh-password），不保证与 ref 的协议段相同。
        let plaintext = format::decrypt(&key, entry, reference.as_str().as_bytes())?;
        match parsed_reference.material() {
            CredentialMaterial::Password => Ok(ResolvedCredential::Password(
                secret_bytes_to_string(plaintext.as_slice())?,
            )),
            CredentialMaterial::PrivateKey => Ok(ResolvedCredential::PrivateKey(plaintext)),
            CredentialMaterial::KeyPassphrase => Ok(ResolvedCredential::KeyPassphrase(
                secret_bytes_to_string(plaintext.as_slice())?,
            )),
        }
    }
}

#[async_trait]
impl CredentialVaultRepository for super::VaultStore {
    /// 将 CPU/磁盘加密工作放入阻塞池，避免 UI 或 MCP Tokio executor 被同步文件操作阻塞。
    async fn save(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> AppResult<CredentialVaultSaveMetadata> {
        let store = self.clone();
        task::spawn_blocking(move || store.save_sync(reference, secret))
            .await
            .map_err(|source| AppError::infrastructure_with_source("保存凭据失败", source))?
            .map_err(|source| to_app_error("保存凭据失败", source))
    }

    /// 只读取加密索引并在阻塞池执行，不将 ciphertext 或引用写入日志。
    async fn contains(&self, reference: &CredentialRef) -> AppResult<bool> {
        let store = self.clone();
        let reference = reference.clone();
        task::spawn_blocking(move || store.contains_sync(&reference))
            .await
            .map_err(|source| AppError::infrastructure_with_source("检查凭据状态失败", source))?
            .map_err(|source| to_app_error("检查凭据状态失败", source))
    }

    /// 在 host CAS 成功后原子清理旧 ref，缺失项按幂等结果返回 false。
    async fn remove(&self, reference: &CredentialRef) -> AppResult<bool> {
        let store = self.clone();
        let reference = reference.clone();
        task::spawn_blocking(move || store.remove_sync(&reference))
            .await
            .map_err(|source| AppError::infrastructure_with_source("删除凭据失败", source))?
            .map_err(|source| to_app_error("删除凭据失败", source))
    }
}

#[async_trait]
impl CredentialResolver for super::VaultStore {
    /// 仅在连接调用时解密引用，并在返回或取消后清除临时 plaintext buffer。
    async fn resolve(
        &self,
        reference: &CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("凭据解析已取消"));
        }
        let store = self.clone();
        let reference = reference.clone();
        let resolved = task::spawn_blocking(move || store.resolve_sync(&reference))
            .await
            .map_err(|source| AppError::infrastructure_with_source("解析凭据失败", source))?
            .map_err(|source| to_app_error("解析凭据失败", source))?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("凭据解析已取消"));
        }
        Ok(resolved)
    }
}

/// 解析已验证的 key TOML UTF-8 文本，错误不回显源内容。
pub(super) fn parse_key_bytes(source: &[u8]) -> Result<VaultKeyFile, VaultError> {
    let source = str::from_utf8(source).map_err(|_| VaultError::InvalidData)?;
    format::parse_key(source)
}

/// 将可选 vault 字节解码为 schema 1 内容，缺文件按空 vault 处理。
pub(super) fn parse_vault_bytes(source: Option<&[u8]>) -> Result<VaultFile, VaultError> {
    match source {
        Some(source) => {
            let source = str::from_utf8(source).map_err(|_| VaultError::InvalidData)?;
            format::parse_vault(source)
        }
        None => Ok(VaultFile {
            schema_version: format::VAULT_SCHEMA_VERSION,
            entries: Vec::new(),
        }),
    }
}

/// 把解密后密码/passphrase 转为新的 Zeroizing 字符串，避免普通字符串长期留存。
fn secret_bytes_to_string(bytes: &[u8]) -> Result<Zeroizing<String>, VaultError> {
    let text = str::from_utf8(bytes).map_err(|_| VaultError::InvalidData)?;
    Ok(Zeroizing::new(text.to_owned()))
}
