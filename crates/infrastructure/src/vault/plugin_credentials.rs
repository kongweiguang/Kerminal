//! 插件秘密使用隔离引用并复用 Vault 加密和 pair 恢复事务。
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode, plugin_settings::PluginSecretRepository,
};
use kerminal_domain::plugins::PluginId;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::{
    VaultStore,
    error::{VaultError, to_app_error},
    format,
    store::{parse_key_bytes, parse_vault_bytes},
    transaction::{self, PairUpdate, read_pair},
};

const KIND: &str = "plugin-secret";
const MAX_SECRET_BYTES: usize = 4 * 1024 * 1024;

/// 槽值只参与摘要，避免分隔符和路径字符造成跨槽或跨插件混淆。
fn slot_digest(slot: &str) -> AppResult<String> {
    if slot.is_empty() || slot.len() > 4096 || slot.contains('\0') {
        return Err(AppError::invalid("插件秘密槽无效"));
    }
    Ok(format!("{:x}", Sha256::digest(slot.as_bytes())))
}

/// 插件身份已经由领域类型验证，冒号边界使前缀匹配不能覆盖相近身份。
fn prefix(plugin: &PluginId) -> String {
    format!("credential:kerminal-plugin:{}:", plugin.as_str())
}

/// API 槽稳定映射到一个条目，设置槽则使用带随机版本的不可变候选。
fn api_reference(plugin: &PluginId, slot: &str) -> AppResult<String> {
    Ok(format!("{}api:{}:v1", prefix(plugin), slot_digest(slot)?))
}

/// 新候选不会覆盖旧引用；设置 CAS 未确认时两者均可恢复。
fn setting_reference(plugin: &PluginId, path: &str) -> AppResult<String> {
    Ok(format!(
        "{}settings:{}:{}:v1",
        prefix(plugin),
        slot_digest(path)?,
        uuid::Uuid::new_v4()
    ))
}

/// 引用必须绑定相同插件、字段摘要及合法随机版本，不能借此读取其他槽。
fn owns_setting(plugin: &PluginId, path: &str, reference: &str) -> AppResult<bool> {
    let prefix = format!("{}settings:{}:", prefix(plugin), slot_digest(path)?);
    Ok(reference
        .strip_prefix(&prefix)
        .and_then(|tail| tail.strip_suffix(":v1"))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()))
}

/// 取消只在提交前生效，已提交或结果未知必须由 Vault 回执表达。
fn ensure_active(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("插件秘密操作已取消"))
    } else {
        Ok(())
    }
}

/// 即使目标槽不存在，也先验证已有 pair，避免把缺失 key 或损坏 Vault 误报为空槽。
fn validated_key(
    source: Option<&[u8]>,
    vault_exists: bool,
    vault: &format::VaultFile,
) -> AppResult<Option<format::VaultKeyFile>> {
    match source {
        Some(source) => {
            let key = parse_key_bytes(source)
                .map_err(|error| to_app_error("读取 Vault 密钥失败", error))?;
            super::key_management::validate_key_against_vault(&key, vault)
                .map_err(|error| to_app_error("验证 Vault 失败", error))?;
            Ok(Some(key))
        }
        None if !vault_exists => Ok(None),
        None => Err(to_app_error("读取 Vault 密钥失败", VaultError::MissingKey)),
    }
}

impl VaultStore {
    /// 结果未知时只比较同一引用的已恢复明文，不重放加密写入或删除候选。
    fn save_plugin_entry_readback(
        &self,
        reference: &str,
        secret: Zeroizing<String>,
        unique: bool,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        let expected = secret.clone();
        match self.save_plugin_entry(reference, secret, unique, cancellation) {
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                match self.read_plugin_entry(reference, &CancellationToken::new()) {
                    Ok(Some(actual)) if actual.as_str() == expected.as_str() => Ok(()),
                    _ => Err(error),
                }
            }
            result => result,
        }
    }

    /// 同一存储锁保护 key、密文和候选发布；未知 pair 提交交给现有恢复事务。
    fn save_plugin_entry(
        &self,
        reference: &str,
        secret: Zeroizing<String>,
        unique: bool,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        ensure_active(cancellation)?;
        if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
            return Err(AppError::invalid("插件秘密长度无效"));
        }
        let _guard = self
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("无法锁定 Vault"))?;
        ensure_active(cancellation)?;
        transaction::recover_pending(&self.storage)
            .map_err(|error| to_app_error("恢复插件秘密失败", error))?;
        let (key_source, vault_source) =
            read_pair(&self.storage).map_err(|error| to_app_error("读取插件秘密失败", error))?;
        let mut key = match key_source.as_ref() {
            Some(source) => Some(
                parse_key_bytes(source)
                    .map_err(|error| to_app_error("读取 Vault 密钥失败", error))?,
            ),
            None if vault_source.is_none() => None,
            None => return Err(to_app_error("读取 Vault 密钥失败", VaultError::MissingKey)),
        };
        let mut vault = parse_vault_bytes(vault_source.as_deref().map(|source| source.as_slice()))
            .map_err(|error| to_app_error("读取插件秘密失败", error))?;
        if let Some(key) = key.as_ref() {
            super::key_management::validate_key_against_vault(key, &vault)
                .map_err(|error| to_app_error("验证 Vault 失败", error))?;
        }
        let previous = vault.entries.iter().find(|entry| entry.id == reference);
        if previous.is_some_and(|entry| entry.kind != KIND) {
            return Err(AppError::conflict("插件秘密槽类型不匹配"));
        }
        if unique && previous.is_some() {
            return Err(AppError::conflict("插件秘密候选身份冲突"));
        }
        let created_key = key.is_none();
        if created_key {
            key = Some(
                format::new_key().map_err(|error| to_app_error("创建 Vault 密钥失败", error))?,
            );
        }
        let key = key
            .as_ref()
            .ok_or_else(|| AppError::infrastructure("Vault 密钥不可用"))?;
        let mut entry = format::encrypt(
            key,
            reference,
            KIND,
            reference.as_bytes(),
            secret.as_bytes(),
        )
        .map_err(|error| to_app_error("加密插件秘密失败", error))?;
        if let Some(previous) = previous {
            entry.created_at.clone_from(&previous.created_at);
        }
        vault.entries.retain(|old| old.id != reference);
        vault.entries.push(entry);
        vault.entries.sort_by(|left, right| left.id.cmp(&right.id));
        let vault_source = format::encode_vault(&vault)
            .map_err(|error| to_app_error("编码插件秘密失败", error))?;
        let key_source = if created_key {
            Some(
                format::encode_key(key)
                    .map_err(|error| to_app_error("编码 Vault 密钥失败", error))?,
            )
        } else {
            None
        };
        ensure_active(cancellation)?;
        transaction::commit_pair_updates(
            &self.storage,
            PairUpdate {
                key: key_source.as_ref().map(|source| source.as_bytes()),
                vault: Some(vault_source.as_bytes()),
            },
        )
        .map_err(|error| to_app_error("保存插件秘密失败", error))
    }

    /// 解密前核对密文标签和 AAD，损坏密文不能被误判为缺失槽。
    fn read_plugin_entry(
        &self,
        reference: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<Zeroizing<String>>> {
        ensure_active(cancellation)?;
        let _guard = self
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("无法锁定 Vault"))?;
        ensure_active(cancellation)?;
        transaction::recover_pending(&self.storage)
            .map_err(|error| to_app_error("恢复插件秘密失败", error))?;
        let (key_source, vault_source) =
            read_pair(&self.storage).map_err(|error| to_app_error("读取插件秘密失败", error))?;
        let vault = parse_vault_bytes(vault_source.as_deref().map(|source| source.as_slice()))
            .map_err(|error| to_app_error("读取插件秘密失败", error))?;
        let key = validated_key(
            key_source.as_deref().map(|source| source.as_slice()),
            vault_source.is_some(),
            &vault,
        )?;
        let Some(entry) = vault.entries.iter().find(|entry| entry.id == reference) else {
            return Ok(None);
        };
        if entry.kind != KIND {
            return Err(AppError::conflict("插件秘密槽类型不匹配"));
        }
        let key = key
            .as_ref()
            .ok_or_else(|| to_app_error("读取 Vault 密钥失败", VaultError::MissingKey))?;
        let plaintext = format::decrypt(key, entry, reference.as_bytes())
            .map_err(|error| to_app_error("解密插件秘密失败", error))?;
        let text = std::str::from_utf8(&plaintext)
            .map_err(|_| AppError::configuration_corrupt("插件秘密编码"))?;
        ensure_active(cancellation)?;
        Ok(Some(Zeroizing::new(text.to_owned())))
    }

    /// 批量删除只处理完全匹配的插件前缀，其他 kind 冲突时保留全部原条目。
    fn delete_plugin_entries(
        &self,
        selector: &str,
        exact: bool,
        cancellation: &CancellationToken,
    ) -> AppResult<bool> {
        ensure_active(cancellation)?;
        let _guard = self
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("无法锁定 Vault"))?;
        ensure_active(cancellation)?;
        transaction::recover_pending(&self.storage)
            .map_err(|error| to_app_error("恢复插件秘密失败", error))?;
        let (key_source, vault_source) =
            read_pair(&self.storage).map_err(|error| to_app_error("读取插件秘密失败", error))?;
        let mut vault = parse_vault_bytes(vault_source.as_deref().map(|source| source.as_slice()))
            .map_err(|error| to_app_error("读取插件秘密失败", error))?;
        validated_key(
            key_source.as_deref().map(|source| source.as_slice()),
            vault_source.is_some(),
            &vault,
        )?;
        let matches = |id: &str| {
            if exact {
                id == selector
            } else {
                id.starts_with(selector)
            }
        };
        if vault
            .entries
            .iter()
            .any(|entry| matches(&entry.id) && entry.kind != KIND)
        {
            return Err(AppError::conflict("插件秘密槽类型不匹配"));
        }
        if !vault.entries.iter().any(|entry| matches(&entry.id)) {
            return Ok(false);
        }
        vault.entries.retain(|entry| !matches(&entry.id));
        let source =
            format::encode_vault(&vault).map_err(|error| to_app_error("编码 Vault 失败", error))?;
        ensure_active(cancellation)?;
        transaction::commit_pair_updates(
            &self.storage,
            PairUpdate {
                key: None,
                vault: Some(source.as_bytes()),
            },
        )
        .map_err(|error| to_app_error("删除插件秘密失败", error))?;
        Ok(true)
    }
}

#[async_trait]
impl PluginSecretRepository for VaultStore {
    /// 候选引用在任务开始前固定，工作线程失败时返回未知结果而不重放。
    async fn create_setting(
        &self,
        plugin: &PluginId,
        path: &str,
        secret: Zeroizing<String>,
        cancellation: CancellationToken,
    ) -> AppResult<String> {
        let reference = setting_reference(plugin, path)?;
        let saved = reference.clone();
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.save_plugin_entry_readback(&saved, secret, true, &cancellation)
        })
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("插件秘密结果未知，请读回核对", error)
        })??;
        Ok(reference)
    }

    /// 字段引用由宿主校验身份后解密，缺少候选不是空字符串。
    async fn read_setting(
        &self,
        plugin: &PluginId,
        path: &str,
        reference: &str,
        cancellation: CancellationToken,
    ) -> AppResult<Zeroizing<String>> {
        if !owns_setting(plugin, path, reference)? {
            return Err(AppError::invalid("插件秘密引用不属于此字段"));
        }
        let reference = reference.to_owned();
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.read_plugin_entry(&reference, &cancellation))
            .await
            .map_err(|error| AppError::infrastructure_with_source("读取插件秘密任务失败", error))??
            .ok_or_else(|| AppError::configuration_corrupt("插件秘密引用缺失"))
    }

    /// 固定槽的哈希由宿主计算，调用者不能注入别的插件引用。
    async fn get(
        &self,
        plugin: &PluginId,
        slot: &str,
        cancellation: CancellationToken,
    ) -> AppResult<Option<Zeroizing<String>>> {
        let reference = api_reference(plugin, slot)?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.read_plugin_entry(&reference, &cancellation))
            .await
            .map_err(|error| AppError::infrastructure_with_source("读取插件秘密任务失败", error))?
    }

    /// 覆盖调用在共享阻塞池完成；未知结果不被转换成普通失败。
    async fn set(
        &self,
        plugin: &PluginId,
        slot: &str,
        secret: Zeroizing<String>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let reference = api_reference(plugin, slot)?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.save_plugin_entry_readback(&reference, secret, false, &cancellation)
        })
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("插件秘密结果未知，请读回核对", error)
        })?
    }

    /// 槽删除与其他 Vault 更新共用 pair 事务，缺失条目返回 false。
    async fn delete(
        &self,
        plugin: &PluginId,
        slot: &str,
        cancellation: CancellationToken,
    ) -> AppResult<bool> {
        let reference = api_reference(plugin, slot)?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.delete_plugin_entries(&reference, true, &cancellation)
        })
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("插件秘密删除结果未知，请读回核对", error)
        })?
    }

    /// 显式删除时清除同一插件的候选与 API 槽，已卸载插件仍可按身份执行。
    async fn delete_all(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let reference = prefix(plugin);
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.delete_plugin_entries(&reference, false, &cancellation)
        })
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("插件秘密删除结果未知，请读回核对", error)
        })??;
        Ok(())
    }
}
