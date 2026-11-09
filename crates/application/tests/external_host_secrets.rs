//! 主机秘密只在指定主机和字段范围内解密，保存测试不得产生配置副作用。
//! @author kongweiguang
use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::host_types::ExternalHostSecretService,
    plugin_settings::{PluginSecretEdit, PluginSecretRepository},
};
use kerminal_domain::{identity::HostId, plugins::PluginId};
use kerminal_plugin_protocol::ConfigurationSchema;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[derive(Default)]
struct Vault {
    values: Mutex<BTreeMap<String, (String, String)>>,
}
#[async_trait]
impl PluginSecretRepository for Vault {
    /// 测试候选绑定槽身份，模拟真实 Vault 的不可变引用。
    async fn create_setting(
        &self,
        _: &PluginId,
        path: &str,
        secret: Zeroizing<String>,
        _: CancellationToken,
    ) -> AppResult<String> {
        let reference = format!(
            "credential:kerminal-plugin:acme.example:settings:{}:00000000-0000-4000-8000-000000000000:v1",
            "a".repeat(64)
        );
        self.values
            .lock()
            .unwrap()
            .insert(reference.clone(), (path.to_owned(), secret.to_string()));
        Ok(reference)
    }
    /// 测试替换主机身份时拒绝读取其他槽。
    async fn read_setting(
        &self,
        _: &PluginId,
        path: &str,
        reference: &str,
        _: CancellationToken,
    ) -> AppResult<Zeroizing<String>> {
        let values = self.values.lock().unwrap();
        let (owner, value) = values
            .get(reference)
            .ok_or_else(|| AppError::invalid("missing"))?;
        if owner != path {
            return Err(AppError::invalid("owner mismatch"));
        }
        Ok(Zeroizing::new(value.clone()))
    }
    /// 测试不使用通用插件 API 槽。
    async fn get(
        &self,
        _: &PluginId,
        _: &str,
        _: CancellationToken,
    ) -> AppResult<Option<Zeroizing<String>>> {
        unreachable!()
    }
    /// 测试不使用通用插件 API 槽。
    async fn set(
        &self,
        _: &PluginId,
        _: &str,
        _: Zeroizing<String>,
        _: CancellationToken,
    ) -> AppResult<()> {
        unreachable!()
    }
    /// 测试不使用通用插件 API 槽。
    async fn delete(&self, _: &PluginId, _: &str, _: CancellationToken) -> AppResult<bool> {
        unreachable!()
    }
    /// 测试不使用全量清除。
    async fn delete_all(&self, _: &PluginId, _: CancellationToken) -> AppResult<()> {
        unreachable!()
    }
}

/// 新秘密先生成引用，草稿测试只读旧值，跨主机不能复用引用。
#[tokio::test]
async fn host_secret_is_scoped_and_preview_does_not_write() {
    let vault = Arc::new(Vault::default());
    let schema = ConfigurationSchema::parse(br#"{"type":"object","properties":{"token":{"type":"string","minLength":3,"x-secret":true}},"required":["token"]}"#).unwrap();
    let service = ExternalHostSecretService::new(
        PluginId::parse("acme.example").unwrap(),
        schema,
        vault.clone(),
    );
    let host = HostId::new();
    let mut edits = BTreeMap::new();
    edits.insert(
        "/token".into(),
        PluginSecretEdit::Replace(Zeroizing::new("secret".into())),
    );
    let saved = service
        .prepare(
            &host,
            json!({"token":null}),
            None,
            edits,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!saved.to_string().contains("secret\""));
    let (values, secrets) = service
        .preview(
            &host,
            json!({"token":null}),
            Some(&saved),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(values["token"].is_null());
    assert_eq!(secrets["/token"], "secret");
    assert_eq!(vault.values.lock().unwrap().len(), 1);
    assert!(
        service
            .preview(
                &HostId::new(),
                json!({"token":null}),
                Some(&saved),
                BTreeMap::new(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
}

/// 可选秘密的移除只影响本次草稿，预览不能修改已有候选或泄漏秘密到普通配置。
#[tokio::test]
async fn preview_removes_optional_secret_without_writing_vault() {
    let vault = Arc::new(Vault::default());
    let schema = ConfigurationSchema::parse(
        br#"{"type":"object","properties":{"token":{"type":"string","x-secret":true}}}"#,
    )
    .unwrap();
    let service = ExternalHostSecretService::new(
        PluginId::parse("acme.example").unwrap(),
        schema,
        vault.clone(),
    );
    let host = HostId::new();
    let mut edits = BTreeMap::new();
    edits.insert(
        "/token".into(),
        PluginSecretEdit::Replace(Zeroizing::new("secret".into())),
    );
    let saved = service
        .prepare(
            &host,
            json!({"token":null}),
            None,
            edits,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut remove = BTreeMap::new();
    remove.insert("/token".into(), PluginSecretEdit::Remove);
    let (config, secrets) = service
        .preview(
            &host,
            json!({"token":null}),
            Some(&saved),
            remove,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(config.get("token").is_none());
    assert!(secrets.is_empty());
    assert_eq!(vault.values.lock().unwrap().len(), 1);
}

/// 必填秘密的移除必须被 schema 拒绝，原始候选仍可供撤销操作读取。
#[tokio::test]
async fn preview_rejects_removing_required_secret() {
    let vault = Arc::new(Vault::default());
    let schema = ConfigurationSchema::parse(br#"{"type":"object","properties":{"token":{"type":"string","x-secret":true}},"required":["token"]}"#).unwrap();
    let service = ExternalHostSecretService::new(
        PluginId::parse("acme.example").unwrap(),
        schema,
        vault.clone(),
    );
    let host = HostId::new();
    let mut edits = BTreeMap::new();
    edits.insert(
        "/token".into(),
        PluginSecretEdit::Replace(Zeroizing::new("secret".into())),
    );
    let saved = service
        .prepare(
            &host,
            json!({"token":null}),
            None,
            edits,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut remove = BTreeMap::new();
    remove.insert("/token".into(), PluginSecretEdit::Remove);
    assert!(
        service
            .preview(
                &host,
                json!({"token":null}),
                Some(&saved),
                remove,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(vault.values.lock().unwrap().len(), 1);
}
