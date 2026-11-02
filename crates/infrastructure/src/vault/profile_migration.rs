//! 迁移期离线密封本地主机环境，复用 Vault 加密与权限策略。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, fs, io, path::Path};

use kerminal_domain::{
    connections::credentials::{
        CredentialKind, CredentialMaterial, CredentialReference, CredentialScope,
    },
    identity::HostId,
};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Value};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::storage::StorageCoordinator;

use super::{
    error::VaultError,
    format::{self, VaultEntry},
    key_management::validate_key_against_vault,
    store::{parse_key_bytes, parse_vault_bytes},
    transaction,
};

/// 只返回已加密内容；主密钥在离开规划器之前始终置于自动清零容器。
pub(crate) struct SealedProfileEnvironments {
    pub(crate) references: BTreeMap<String, String>,
    pub(crate) new_key: Option<Zeroizing<Vec<u8>>>,
    pub(crate) vault_before: Option<Zeroizing<Vec<u8>>>,
    pub(crate) vault_after: Vec<u8>,
}

/// 在内存中生成一批引用和密文；只读预检不创建 Vault、目录、锁或密钥文件。
pub(crate) fn seal_profile_environments(
    root: &Path,
    environments: &BTreeMap<String, &BTreeMap<String, String>>,
    normalized_vault: Option<&[u8]>,
) -> Result<Option<SealedProfileEnvironments>, VaultError> {
    if environments
        .values()
        .all(|environment| environment.is_empty())
    {
        return Ok(None);
    }
    reject_pending_pair(root)?;
    let storage = StorageCoordinator::new(root);
    let (key_source, vault_source) = transaction::read_pair_readonly(&storage)?;
    let created_key = key_source.is_none();
    let key = match key_source.as_ref() {
        Some(source) => parse_key_bytes(source.as_slice())?,
        None if vault_source.is_none() => format::new_key()?,
        None => return Err(VaultError::MissingKey),
    };
    let vault_contents = normalized_vault.or_else(|| vault_source.as_deref().map(Vec::as_slice));
    let vault = parse_vault_bytes(vault_contents)?;
    validate_key_against_vault(&key, &vault)?;

    let mut references = BTreeMap::new();
    let mut new_entries = Vec::new();
    for (host_id, environment) in environments {
        if environment.is_empty() {
            continue;
        }
        let host_id_value = HostId::parse(host_id).map_err(|_| VaultError::InvalidData)?;
        let reference = loop {
            let scope = format!("environment-{}", Uuid::new_v4().simple());
            let candidate = CredentialReference::new(
                CredentialKind::parse("local-host").map_err(|_| VaultError::InvalidData)?,
                host_id_value.clone(),
                CredentialScope::parse(&scope).map_err(|_| VaultError::InvalidData)?,
                CredentialMaterial::Password,
            )
            .map_err(|_| VaultError::InvalidData)?;
            if vault
                .entries
                .iter()
                .all(|entry| entry.id != candidate.reference().as_str())
                && new_entries
                    .iter()
                    .all(|entry: &VaultEntry| entry.id != candidate.reference().as_str())
            {
                break candidate;
            }
        };
        let plaintext =
            Zeroizing::new(serde_json::to_vec(environment).map_err(|_| VaultError::InvalidData)?);
        if plaintext.len() > 4 * 1024 * 1024 {
            return Err(VaultError::InvalidData);
        }
        let reference_text = reference.reference().as_str();
        let entry = format::encrypt(
            &key,
            reference_text,
            reference.kind().as_str(),
            reference_text.as_bytes(),
            plaintext.as_slice(),
        )?;
        references.insert(host_id.clone(), reference_text.to_owned());
        new_entries.push(entry);
    }
    let vault_after = append_entries(vault_contents, &new_entries)?;
    let decoded = parse_vault_bytes(Some(&vault_after))?;
    validate_key_against_vault(&key, &decoded)?;
    let new_key = if created_key {
        Some(Zeroizing::new(
            format::encode_key(&key)?.as_bytes().to_vec(),
        ))
    } else {
        None
    };
    Ok(Some(SealedProfileEnvironments {
        references,
        new_key,
        vault_before: vault_source,
        vault_after,
    }))
}

/// 重复导入时按 Vault 原始引用解密比较环境，不能覆盖已被用户修改的本地主机。
pub(crate) fn profile_environment_matches(
    root: &Path,
    host_id: &str,
    reference: Option<&str>,
    expected: &BTreeMap<String, String>,
    normalized_vault: Option<&[u8]>,
) -> Result<bool, VaultError> {
    let Some(reference) = reference else {
        return Ok(expected.is_empty());
    };
    reject_pending_pair(root)?;
    let reference = kerminal_domain::connections::CredentialRef::new(reference.to_owned())
        .map_err(|_| VaultError::InvalidData)?;
    let parsed =
        CredentialReference::parse(reference.clone()).map_err(|_| VaultError::InvalidData)?;
    if parsed.kind().as_str() != "local-host" || parsed.host_id().as_str() != host_id {
        return Err(VaultError::InvalidData);
    }
    let storage = StorageCoordinator::new(root);
    let (key_source, vault_source) = transaction::read_pair_readonly(&storage)?;
    let key = parse_key_bytes(
        key_source
            .as_ref()
            .ok_or(VaultError::MissingKey)?
            .as_slice(),
    )?;
    let vault_bytes = normalized_vault.or_else(|| vault_source.as_deref().map(Vec::as_slice));
    let vault = parse_vault_bytes(vault_bytes)?;
    validate_key_against_vault(&key, &vault)?;
    let entry = vault
        .entries
        .iter()
        .find(|entry| entry.id == reference.as_str())
        .ok_or(VaultError::MissingEntry)?;
    let plaintext = format::decrypt(&key, entry, reference.as_str().as_bytes())?;
    let mut actual: BTreeMap<String, String> =
        serde_json::from_slice(&plaintext).map_err(|_| VaultError::InvalidData)?;
    let matches = &actual == expected;
    for value in actual.values_mut() {
        zeroize::Zeroize::zeroize(value);
    }
    Ok(matches)
}

/// 在原 TOML 上追加密文条目，保留已有条目顺序、未知字段和用户注释。
fn append_entries(source: Option<&[u8]>, entries: &[VaultEntry]) -> Result<Vec<u8>, VaultError> {
    let text = match source {
        Some(bytes) => std::str::from_utf8(bytes).map_err(|_| VaultError::InvalidData)?,
        None => "schema_version = 1\n",
    };
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|_| VaultError::InvalidData)?;
    if document.get("entries").is_none() {
        document["entries"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let target = document.get_mut("entries").ok_or(VaultError::InvalidData)?;
    for entry in entries {
        let encoded = toml_edit::ser::to_document(entry).map_err(|_| VaultError::InvalidData)?;
        let table = encoded.into_table();
        match &mut *target {
            Item::ArrayOfTables(tables) => tables.push(table),
            Item::Value(Value::Array(array)) => {
                array.push(Value::InlineTable(table.into_inline_table()))
            }
            _ => return Err(VaultError::InvalidData),
        }
    }
    Ok(document.to_string().into_bytes())
}

/// 未完成的 Vault 配对事务必须先由原 owner 恢复，不能在不一致快照上叠加迁移。
fn reject_pending_pair(root: &Path) -> Result<(), VaultError> {
    let secrets = root.join("secrets");
    let transactions = secrets.join(".vault-transaction");
    for directory in [&secrets, &transactions] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Ok(_) => return Err(VaultError::UnsafePath),
            Err(error) => return Err(VaultError::from(error)),
        }
    }
    let path = transactions.join("active");
    match fs::symlink_metadata(path) {
        Ok(_) => Err(VaultError::RecoveryRequired),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(VaultError::from(error)),
    }
}

/// 迁移备份可能含旧 Profile 明文，故所有新建事务文件沿用 Vault 的私有创建模式。
pub(crate) fn private_creation_mode(options: &mut fs::OpenOptions) {
    transaction::set_private_creation_mode(options);
}

/// 在写入任何敏感内容前设置 owner-only 文件 ACL，避免失败时留下公开的半成品。
pub(crate) fn secure_file(path: &Path) -> Result<(), VaultError> {
    transaction::set_private_file_permissions(path)
}

/// 限定事务或 secrets 目录的访问者，并对调用方保留底层失败分类。
pub(crate) fn secure_directory(path: &Path) -> Result<(), VaultError> {
    transaction::set_private_directory_permissions(path)
}

/// 权限应用失败时仅移除本次刚独占创建的空文件，不修改既有用户数据。
pub(crate) fn secure_new_file(path: &Path) -> Result<(), VaultError> {
    if let Err(error) = secure_file(path) {
        if let Err(remove_error) = fs::remove_file(path)
            && remove_error.kind() != io::ErrorKind::NotFound
        {
            return Err(VaultError::from(remove_error));
        }
        return Err(error);
    }
    Ok(())
}
