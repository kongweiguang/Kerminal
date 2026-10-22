//! Vault schema 1 的一次性 AAD 元数据修复；普通运行时 reader 不承担旧字段兜底。
//!
//! @author kongweiguang

use std::{collections::HashSet, fs, io, path::Path, str};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::connections::{
    CredentialRef,
    credentials::{CredentialKind, CredentialReference},
};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Value};

use super::FileEdit;

const VAULT_RELATIVE_PATH: &str = "secrets/vault.toml";
const CURRENT_ALGORITHM: &str = "xchacha20poly1305";
const NONCE_BYTES: usize = 24;
const AUTH_TAG_BYTES: usize = 16;

/// 只反序列化检查 schema 字段；未知 TOML 字段由 DocumentMut 原位保留。
#[derive(Deserialize)]
struct VaultDocument {
    schema_version: u32,
    #[serde(default)]
    entries: Vec<VaultEntryDocument>,
}

/// 描述 schema 1 的必要密文元数据，不读取 key 或尝试解密凭据正文。
#[derive(Deserialize)]
struct VaultEntryDocument {
    id: String,
    kind: String,
    algorithm: String,
    key_id: String,
    #[serde(default)]
    associated_data: Option<String>,
    nonce: String,
    ciphertext: String,
    created_at: String,
    updated_at: String,
}

/// 只为明确旧格式补 AAD 的可逆元数据表示，密钥、nonce 与 ciphertext 都不参与改写。
struct AadUpdate {
    id: String,
    encoded: String,
}

/// 对固定 vault 文件规划最窄编辑；磁盘备份、CAS 和提交统一由 migration transaction 执行。
///
/// @author kongweiguang
pub(super) fn plan_vault_migration(root: &Path) -> AppResult<Vec<FileEdit>> {
    let Some(before) = read_vault(root)? else {
        return Ok(Vec::new());
    };
    let source = str::from_utf8(&before).map_err(|_| invalid_vault())?;
    let parsed: VaultDocument = toml::from_str(source).map_err(|_| invalid_vault())?;
    if parsed.schema_version != 1 {
        return Err(invalid_vault());
    }

    let mut ids = HashSet::with_capacity(parsed.entries.len());
    let mut updates = Vec::new();
    for entry in &parsed.entries {
        if !ids.insert(entry.id.as_str()) {
            return Err(invalid_vault());
        }
        if let Some(encoded) = validate_entry(entry)? {
            updates.push(AadUpdate {
                id: entry.id.clone(),
                encoded,
            });
        }
    }
    if updates.is_empty() {
        return Ok(Vec::new());
    }

    let mut document = source.parse::<DocumentMut>().map_err(|_| invalid_vault())?;
    apply_aad_updates(&mut document, &parsed.entries, &updates)?;
    let after = document.to_string().into_bytes();
    if after == before {
        return Err(invalid_vault());
    }
    Ok(vec![FileEdit {
        relative_path: VAULT_RELATIVE_PATH.into(),
        before: Some(before),
        after,
    }])
}

/// 校验每条密文引用与 host 值对象，并仅为缺失 AAD 生成当前 writer 的等价编码。
///
/// @author kongweiguang
fn validate_entry(entry: &VaultEntryDocument) -> AppResult<Option<String>> {
    let reference = CredentialRef::new(entry.id.clone()).map_err(|_| invalid_vault())?;
    let parsed_reference = CredentialReference::parse(reference).map_err(|_| invalid_vault())?;
    CredentialKind::parse(&entry.kind).map_err(|_| invalid_vault())?;
    if parsed_reference.reference().as_str() != entry.id
        || entry.algorithm != CURRENT_ALGORITHM
        || entry.key_id.trim().is_empty()
        || entry.created_at.trim().is_empty()
        || entry.updated_at.trim().is_empty()
    {
        return Err(invalid_vault());
    }

    let nonce = BASE64.decode(&entry.nonce).map_err(|_| invalid_vault())?;
    let ciphertext = BASE64
        .decode(&entry.ciphertext)
        .map_err(|_| invalid_vault())?;
    if nonce.len() != NONCE_BYTES || ciphertext.len() < AUTH_TAG_BYTES {
        return Err(invalid_vault());
    }

    match entry.associated_data.as_deref() {
        Some(value) if !value.is_empty() => {
            let decoded = BASE64.decode(value).map_err(|_| invalid_vault())?;
            if decoded != entry.id.as_bytes() {
                return Err(invalid_vault());
            }
            Ok(None)
        }
        _ => Ok(Some(BASE64.encode(entry.id.as_bytes()))),
    }
}

/// 只修改 entries 中指定的 AAD 值，并保留 TOML decor、未知字段、注释和其它条目顺序。
///
/// @author kongweiguang
fn apply_aad_updates(
    document: &mut DocumentMut,
    entries: &[VaultEntryDocument],
    updates: &[AadUpdate],
) -> AppResult<()> {
    let update_by_id = updates
        .iter()
        .map(|update| (update.id.as_str(), update.encoded.as_str()))
        .collect::<std::collections::HashMap<_, _>>();
    let Some(item) = document.get_mut("entries") else {
        return Err(invalid_vault());
    };

    match item {
        Item::ArrayOfTables(tables) => {
            if tables.len() != entries.len() {
                return Err(invalid_vault());
            }
            for (table, parsed) in tables.iter_mut().zip(entries) {
                let id = table
                    .get("id")
                    .and_then(Item::as_str)
                    .ok_or_else(invalid_vault)?;
                if id != parsed.id {
                    return Err(invalid_vault());
                }
                if let Some(encoded) = update_by_id.get(id) {
                    set_table_string(table, "associated_data", encoded)?;
                }
            }
        }
        Item::Value(Value::Array(array)) => {
            if array.len() != entries.len() {
                return Err(invalid_vault());
            }
            for (value, parsed) in array.iter_mut().zip(entries) {
                let table = value.as_inline_table_mut().ok_or_else(invalid_vault)?;
                let id = table
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_vault)?;
                if id != parsed.id {
                    return Err(invalid_vault());
                }
                if let Some(encoded) = update_by_id.get(id) {
                    set_inline_string(table, "associated_data", encoded);
                }
            }
        }
        _ => return Err(invalid_vault()),
    }
    Ok(())
}

/// 更新标准 TOML 表字段时保留原字符串的空白与行尾注释，缺失字段只插入一项。
///
/// @author kongweiguang
fn set_table_string(table: &mut toml_edit::Table, key: &str, replacement: &str) -> AppResult<()> {
    if let Some(item) = table.get_mut(key) {
        let value = item.as_value_mut().ok_or_else(invalid_vault)?;
        let decor = value.decor().clone();
        *value = Value::from(replacement);
        value.decor_mut().clone_from(&decor);
    } else {
        table.insert(key, toml_edit::value(replacement));
    }
    Ok(())
}

/// 更新 inline table 中的 AAD，同时沿用现有值的行内装饰。
///
/// @author kongweiguang
fn set_inline_string(table: &mut toml_edit::InlineTable, key: &str, replacement: &str) {
    if let Some(value) = table.get_mut(key) {
        let decor = value.decor().clone();
        *value = Value::from(replacement);
        value.decor_mut().clone_from(&decor);
    } else {
        table.insert(key, Value::from(replacement));
    }
}

/// 仅读取 secrets/vault.toml；不打开或推导 vault-key.toml，保证无 key 也能完成元数据迁移。
///
/// @author kongweiguang
fn read_vault(root: &Path) -> AppResult<Option<Vec<u8>>> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage_error("读取凭据库根目录失败", error)),
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(invalid_vault());
    }

    let secrets_path = root.join("secrets");
    let secrets_metadata = match fs::symlink_metadata(&secrets_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage_error("读取凭据目录失败", error)),
    };
    if secrets_metadata.file_type().is_symlink() || !secrets_metadata.is_dir() {
        return Err(invalid_vault());
    }

    let vault_path = root.join(VAULT_RELATIVE_PATH);
    let vault_metadata = match fs::symlink_metadata(&vault_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage_error("读取凭据库文件状态失败", error)),
    };
    if vault_metadata.file_type().is_symlink() || !vault_metadata.is_file() {
        return Err(invalid_vault());
    }
    let source =
        fs::read(vault_path).map_err(|error| storage_error("读取凭据库文件失败", error))?;
    Ok(Some(source))
}

/// 迁移错误只输出静态分类，不把 credential ref、密文或本机路径写入日志。
///
/// @author kongweiguang
fn invalid_vault() -> AppError {
    AppError::invalid("凭据 vault 不符合当前 schema 1 格式")
}

/// 文件系统错误保留底层诊断类型，但上下文不包含用户数据正文。
///
/// @author kongweiguang
fn storage_error(context: &'static str, source: io::Error) -> AppError {
    AppError::infrastructure_with_source(context, source)
}
