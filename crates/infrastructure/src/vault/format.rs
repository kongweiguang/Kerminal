//! 当前 schema 1 的 vault 序列化与加密格式；历史字段修复只发生在一次性迁移边界。
//!
//! @author kongweiguang

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use super::error::VaultError;

pub(super) const KEY_SCHEMA_VERSION: u32 = 1;
pub(super) const VAULT_SCHEMA_VERSION: u32 = 1;
pub(super) const VAULT_ALGORITHM: &str = "xchacha20poly1305";
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 24;

/// 使用 serde TOML 字符串字段但在内存中及时清除 master key 编码。
pub(super) struct SecretString(Zeroizing<String>);

/// key schema 1 的内部表示；故意不实现 Debug，避免误打印 master key。
#[derive(Serialize, Deserialize)]
pub(super) struct VaultKeyFile {
    pub schema_version: u32,
    pub key_id: String,
    pub algorithm: String,
    pub created_at: String,
    pub master_key: SecretString,
}

/// 当前 vault schema 1 根对象，字段结构稳定并拒绝未知版本。
#[derive(Serialize, Deserialize)]
pub(super) struct VaultFile {
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<VaultEntry>,
}

/// 单条 schema 1 密文；不实现 Debug 以免日志暴露凭据索引或完整密文。
#[derive(Serialize, Deserialize)]
pub(super) struct VaultEntry {
    pub id: String,
    pub kind: String,
    pub algorithm: String,
    pub key_id: String,
    #[serde(default)]
    pub associated_data: String,
    pub nonce: String,
    pub ciphertext: String,
    pub created_at: String,
    pub updated_at: String,
}

impl SecretString {
    /// 将 serde 解码出的 master key 文本放入 zeroize 存储。
    pub(super) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// 读取 key 编码只用于 Base64 解码或 TOML 序列化。
    pub(super) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Serialize for SecretString {
    /// 按旧版 key TOML 输出 Base64 文本，不将内部容器暴露到通用 DTO。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SecretString {
    /// 反序列化后立即把 master key 文本放进自动清除的包装。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

impl std::fmt::Debug for SecretString {
    /// 防止通过容器调试输出间接打印 master key。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([redacted])")
    }
}

/// 解码并验证 master key 字段，返回释放时清零的原始 32 字节 key。
pub(super) fn decode_key(key: &VaultKeyFile) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if key.schema_version != KEY_SCHEMA_VERSION
        || key.key_id.trim().is_empty()
        || key.algorithm != VAULT_ALGORITHM
        || key.created_at.trim().is_empty()
    {
        return Err(VaultError::InvalidData);
    }
    let decoded = Zeroizing::new(
        BASE64
            .decode(key.master_key.as_str())
            .map_err(|_| VaultError::InvalidData)?,
    );
    if decoded.len() != KEY_BYTES {
        return Err(VaultError::InvalidData);
    }
    Ok(decoded)
}

/// 生成当前 key schema 1 的随机 XChaCha20-Poly1305 master key。
pub(super) fn new_key() -> Result<VaultKeyFile, VaultError> {
    let mut bytes = Zeroizing::new(vec![0_u8; KEY_BYTES]);
    getrandom::fill(bytes.as_mut_slice()).map_err(|_| VaultError::InvalidData)?;
    Ok(VaultKeyFile {
        schema_version: KEY_SCHEMA_VERSION,
        key_id: "workspace-default".to_owned(),
        algorithm: VAULT_ALGORITHM.to_owned(),
        created_at: timestamp(),
        master_key: SecretString::new(BASE64.encode(bytes.as_slice())),
    })
}

/// 按当前 schema 1 的 XChaCha20-Poly1305 + Base64 编码写入单条密文记录。
pub(super) fn encrypt(
    key: &VaultKeyFile,
    entry_id: &str,
    kind: &str,
    associated_data: &[u8],
    plaintext: &[u8],
) -> Result<VaultEntry, VaultError> {
    let key_bytes = decode_key(key)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key_bytes.as_slice())
        .map_err(|_| VaultError::InvalidData)?;
    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| VaultError::InvalidData)?;
    let nonce = XNonce::from(nonce);
    let encrypted = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: associated_data,
            },
        )
        .map_err(|_| VaultError::InvalidData)?;
    let now = timestamp();
    Ok(VaultEntry {
        id: entry_id.to_owned(),
        kind: kind.to_owned(),
        algorithm: key.algorithm.clone(),
        key_id: key.key_id.clone(),
        associated_data: BASE64.encode(associated_data),
        nonce: BASE64.encode(nonce),
        ciphertext: BASE64.encode(encrypted),
        created_at: now.clone(),
        updated_at: now,
    })
}

/// 按当前 schema 1 的引用绑定解密；缺失或错误 AAD 必须先经一次性迁移修复。
pub(super) fn decrypt(
    key: &VaultKeyFile,
    entry: &VaultEntry,
    associated_data: &[u8],
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if entry.algorithm != key.algorithm || entry.key_id != key.key_id {
        return Err(VaultError::KeyMismatch);
    }
    let saved_aad = decode_associated_data(entry)?;
    if saved_aad != associated_data || saved_aad != entry.id.as_bytes() {
        return Err(VaultError::KeyMismatch);
    }
    let key_bytes = decode_key(key)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key_bytes.as_slice())
        .map_err(|_| VaultError::InvalidData)?;
    let nonce = BASE64
        .decode(&entry.nonce)
        .map_err(|_| VaultError::InvalidData)?;
    if nonce.len() != NONCE_BYTES {
        return Err(VaultError::InvalidData);
    }
    let nonce: [u8; NONCE_BYTES] = nonce.try_into().map_err(|_| VaultError::InvalidData)?;
    let nonce = XNonce::from(nonce);
    let ciphertext = BASE64
        .decode(&entry.ciphertext)
        .map_err(|_| VaultError::InvalidData)?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext.as_slice(),
                    aad: associated_data,
                },
            )
            .map_err(|_| VaultError::KeyMismatch)?,
    );
    Ok(plaintext)
}

/// 严格读取持久化 AAD 并核对 entry ID，阻止运行时悄悄兜底解密旧条目。
pub(super) fn associated_data(entry: &VaultEntry) -> Result<Vec<u8>, VaultError> {
    let decoded = decode_associated_data(entry)?;
    if decoded != entry.id.as_bytes() {
        return Err(VaultError::KeyMismatch);
    }
    Ok(decoded)
}

/// 统一拒绝空字段与错误 Base64，让解密、导入校验和密钥轮换共享当前格式边界。
fn decode_associated_data(entry: &VaultEntry) -> Result<Vec<u8>, VaultError> {
    if entry.associated_data.is_empty() {
        return Err(VaultError::InvalidData);
    }
    BASE64
        .decode(&entry.associated_data)
        .map_err(|_| VaultError::InvalidData)
}

/// 读取 schema 1 key TOML，并在任何写入前验证全部必需字段。
pub(super) fn parse_key(source: &str) -> Result<VaultKeyFile, VaultError> {
    let key: VaultKeyFile = toml::from_str(source).map_err(|_| VaultError::InvalidData)?;
    decode_key(&key)?;
    Ok(key)
}

/// 按当前字段顺序和 pretty TOML 语义导出 key 文档。
pub(super) fn encode_key(key: &VaultKeyFile) -> Result<Zeroizing<String>, VaultError> {
    decode_key(key)?;
    toml::to_string_pretty(key)
        .map(Zeroizing::new)
        .map_err(|_| VaultError::InvalidData)
}

/// 读取 schema 1 vault；未知版本或重复 entry id 会 fail closed。
pub(super) fn parse_vault(source: &str) -> Result<VaultFile, VaultError> {
    let vault: VaultFile = toml::from_str(source).map_err(|_| VaultError::InvalidData)?;
    if vault.schema_version != VAULT_SCHEMA_VERSION {
        return Err(VaultError::InvalidData);
    }
    let mut ids = std::collections::HashSet::with_capacity(vault.entries.len());
    if vault
        .entries
        .iter()
        .any(|entry| !ids.insert(entry.id.as_str()))
    {
        return Err(VaultError::InvalidData);
    }
    Ok(vault)
}

/// 按当前 vault schema 1 字段序列化密文集合。
pub(super) fn encode_vault(vault: &VaultFile) -> Result<String, VaultError> {
    toml::to_string_pretty(vault).map_err(|_| VaultError::InvalidData)
}

/// 返回与旧 key/vault 文件一致的 Unix 秒时间戳文本。
pub(super) fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}
