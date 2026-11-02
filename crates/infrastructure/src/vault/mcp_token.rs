//! MCP 服务令牌复用现有 Vault 加密格式与 pair 事务。
//!
//! @author kongweiguang

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use kerminal_application::{
    AppError, AppResult,
    mcp_runtime::{MCP_TOKEN_REFERENCE, McpAccessToken, McpTokenStore},
};
use tokio::task;
use zeroize::Zeroizing;

use super::{
    VaultStore,
    error::{VaultError, to_app_error},
    format,
    store::{parse_key_bytes, parse_vault_bytes},
    transaction::{self, PairUpdate, read_pair},
};

impl VaultStore {
    /// 在同一存储锁中读取或首次创建服务令牌，避免两个设置请求生成不同值。
    fn mcp_token_sync(&self, create: bool) -> Result<Zeroizing<String>, VaultError> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| VaultError::LockUnavailable)?;
        transaction::recover_pending(&self.storage)?;
        let (key_source, vault_source) = read_pair(&self.storage)?;
        let mut key = match key_source.as_ref() {
            Some(source) => Some(parse_key_bytes(source.as_slice())?),
            None if vault_source.is_none() && create => None,
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
        if let Some(entry) = vault
            .entries
            .iter()
            .find(|entry| entry.id == MCP_TOKEN_REFERENCE)
        {
            if entry.kind != "mcp-token" {
                return Err(VaultError::InvalidData);
            }
            let bytes = format::decrypt(key, entry, MCP_TOKEN_REFERENCE.as_bytes())?;
            let token =
                std::str::from_utf8(bytes.as_slice()).map_err(|_| VaultError::InvalidData)?;
            if token.is_empty() {
                return Err(VaultError::InvalidData);
            }
            return Ok(Zeroizing::new(token.to_owned()));
        }
        if !create {
            return Err(VaultError::MissingEntry);
        }
        let mut random = Zeroizing::new([0_u8; 32]);
        getrandom::fill(random.as_mut()).map_err(|_| VaultError::InvalidData)?;
        let token = Zeroizing::new(URL_SAFE_NO_PAD.encode(random.as_slice()));
        let entry = format::encrypt(
            key,
            MCP_TOKEN_REFERENCE,
            "mcp-token",
            MCP_TOKEN_REFERENCE.as_bytes(),
            token.as_bytes(),
        )?;
        vault.entries.push(entry);
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
        Ok(token)
    }
}

#[async_trait]
impl McpTokenStore for VaultStore {
    /// 只在用户首次启用局域网时创建，已有引用缺失时拒绝自动换发。
    async fn load_or_create(&self, reference: Option<&str>) -> AppResult<McpAccessToken> {
        if reference.is_some_and(|reference| reference != MCP_TOKEN_REFERENCE) {
            return Err(AppError::invalid("MCP 令牌引用无效"));
        }
        let create = reference.is_none();
        let store = self.clone();
        let secret = task::spawn_blocking(move || store.mcp_token_sync(create))
            .await
            .map_err(|error| AppError::infrastructure_with_source("MCP 令牌读取失败", error))?
            .map_err(|error| to_app_error("MCP 令牌读取失败", error))?;
        McpAccessToken::new(MCP_TOKEN_REFERENCE.to_owned(), secret)
    }
}
