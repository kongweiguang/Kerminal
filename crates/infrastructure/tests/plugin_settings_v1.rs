//! 插件设置、状态与 Vault 命名空间的隔离回归。
//! @author kongweiguang

use kerminal_application::{
    PublicErrorCode,
    plugin_settings::{
        PluginSecretEdit, PluginSettingsRepository, PluginSettingsSave, PluginSettingsService,
        PluginStateRepository, StoredPluginSettings,
    },
    preferences::{ConfigurationRepository, SettingsRepository},
};
use kerminal_domain::{
    plugins::PluginId,
    preferences::{ConfigDocumentPath, ConfigRevision},
};
use kerminal_infrastructure::{configuration::ConfigurationStore, vault::VaultStore};
use kerminal_plugin_protocol::ConfigurationSchema;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// 所有端口共享同一个 coordinator，模拟桌面组合根的锁和恢复边界。
fn fixture(root: &std::path::Path) -> (ConfigurationStore, PluginSettingsService) {
    let config = ConfigurationStore::initialize(root).expect("初始化隔离配置");
    let vault = VaultStore::open(config.storage_coordinator());
    let service = PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(vault),
    );
    (config, service)
}

/// 引用和组合 schema 中的秘密字段仍必须走同一个协议校验器。
fn schema() -> ConfigurationSchema {
    ConfigurationSchema::parse(br##"{
      "type":"object", "required":["enabled","auth"],
      "properties":{"enabled":{"type":"boolean"},"auth":{"$ref":"#/$defs/auth"}},
      "$defs":{"auth":{"type":"object","required":["token"],"properties":{"token":{"type":"string","minLength":5,"x-secret":true}}}}
    }"##).expect("解析协议 schema")
}

/// 保存只发布随机 Vault 引用，读回与事件均脱敏，陈旧 revision 不能覆盖他人更改。
#[tokio::test]
async fn settings_schema_secret_and_cas_share_the_original_configuration() {
    let root = tempfile::tempdir().unwrap();
    let (config, service) = fixture(root.path());
    let plugin = PluginId::parse("acme.notes").unwrap();
    let mut events = service.subscribe();
    let baseline = service
        .load(&plugin, &schema(), CancellationToken::new())
        .await
        .unwrap();
    let mut edits = BTreeMap::new();
    edits.insert(
        "/auth/token".into(),
        PluginSecretEdit::Replace(Zeroizing::new("private-fixture".into())),
    );
    let saved = service
        .save(
            &plugin,
            &schema(),
            PluginSettingsSave {
                values: json!({"enabled":true,"auth":{"token":null}}),
                secrets: edits,
                expected_revision: baseline.revision,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(saved.values, json!({"enabled":true,"auth":{"token":null}}));
    assert!(saved.secret_paths.contains("/auth/token"));
    assert_eq!(events.recv().await.unwrap().snapshot, saved);
    let raw = fs::read_to_string(root.path().join("settings.toml")).unwrap();
    assert!(raw.contains("credential:kerminal-plugin:acme.notes:settings:"));
    assert!(!raw.contains("private-fixture"));
    assert!(
        !fs::read_to_string(root.path().join("secrets/vault.toml"))
            .unwrap()
            .contains("private-fixture")
    );
    assert_eq!(
        service
            .load(&plugin, &schema(), CancellationToken::new())
            .await
            .unwrap(),
        saved
    );
    let current = config.load_settings().await.unwrap();
    config
        .save_settings(current.settings, current.revision)
        .await
        .unwrap();
    let error = service
        .save(
            &plugin,
            &schema(),
            PluginSettingsSave {
                values: saved.values.clone(),
                secrets: BTreeMap::new(),
                expected_revision: saved.revision,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("stale revision must fail");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
}

/// 任意原文或跨插件引用都不能经由配置设置秘密字段绕过 Vault 隔离。
#[tokio::test]
async fn rejects_plaintext_foreign_references_and_invalid_schema_values() {
    let root = tempfile::tempdir().unwrap();
    let (config, service) = fixture(root.path());
    let plugin = PluginId::parse("acme.notes").unwrap();
    let baseline = service
        .load(&plugin, &schema(), CancellationToken::new())
        .await
        .unwrap();
    let result = service
        .save(
            &plugin,
            &schema(),
            PluginSettingsSave {
                values: json!({"enabled":true,"auth":{"token":"unsafe"}}),
                secrets: BTreeMap::new(),
                expected_revision: baseline.revision.clone(),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    let mut edits = BTreeMap::new();
    edits.insert(
        "/auth/token".into(),
        PluginSecretEdit::Replace(Zeroizing::new("tiny".into())),
    );
    assert!(
        service
            .save(
                &plugin,
                &schema(),
                PluginSettingsSave {
                    values: json!({"enabled":true,"auth":{"token":null}}),
                    secrets: edits,
                    expected_revision: baseline.revision,
                },
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(!root.path().join("secrets/vault.toml").exists());
    let raw = "schema_version = 1\n[plugins.\"acme.notes\"]\nenabled = true\n[plugins.\"acme.notes\".auth]\ntoken = 'credential:kerminal-plugin:other.plugin:settings:forged:v1'\n";
    config
        .write(ConfigDocumentPath::Settings, raw.into(), None)
        .await
        .unwrap();
    assert!(
        service
            .load(&plugin, &schema(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(root.path().join("settings.toml")).unwrap(),
        raw
    );
}

/// 超限、取消和显式删除分别验证状态、API 槽及插件数据目录边界。
#[tokio::test]
async fn state_secrets_and_delete_data_are_isolated() {
    let root = tempfile::tempdir().unwrap();
    let (_config, service) = fixture(root.path());
    let first = PluginId::parse("acme.notes").unwrap();
    let second = PluginId::parse("other.notes").unwrap();
    let schema = ConfigurationSchema::parse(
        br#"{"type":"object","properties":{"token":{"type":"string","x-secret":true}}}"#,
    )
    .unwrap();
    for plugin in [&first, &second] {
        let baseline = service
            .load(plugin, &schema, CancellationToken::new())
            .await
            .unwrap();
        let mut secrets = BTreeMap::new();
        secrets.insert(
            "/token".into(),
            PluginSecretEdit::Replace(Zeroizing::new(plugin.as_str().to_owned())),
        );
        service
            .save(
                plugin,
                &schema,
                PluginSettingsSave {
                    values: json!({"token":null}),
                    secrets,
                    expected_revision: baseline.revision,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        service
            .state_set(
                plugin,
                json!({"plugin":plugin.as_str()}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        service
            .secret_set(
                plugin,
                "shared",
                Zeroizing::new(plugin.as_str().to_string()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        service
            .secret_get(&first, "shared", CancellationToken::new())
            .await
            .unwrap()
            .unwrap()
            .as_str(),
        "acme.notes"
    );
    assert_eq!(
        service
            .secret_get(&second, "shared", CancellationToken::new())
            .await
            .unwrap()
            .unwrap()
            .as_str(),
        "other.notes"
    );
    let oversized = Value::String("x".repeat(1024 * 1024));
    assert!(
        service
            .state_set(&first, oversized, CancellationToken::new())
            .await
            .is_err()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        service
            .secret_delete(&first, "shared", cancelled)
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::Cancelled
    );
    fs::write(
        root.path().join("plugins/data/acme.notes/extra.txt"),
        "data",
    )
    .unwrap();
    service
        .delete_data(&first, CancellationToken::new())
        .await
        .unwrap();
    assert!(!root.path().join("plugins/data/acme.notes").exists());
    let remaining_settings = fs::read_to_string(root.path().join("settings.toml")).unwrap();
    let remaining_vault = fs::read_to_string(root.path().join("secrets/vault.toml")).unwrap();
    assert!(!remaining_settings.contains("acme.notes"));
    assert!(!remaining_vault.contains("credential:kerminal-plugin:acme.notes:"));
    assert!(remaining_settings.contains("other.notes"));
    assert!(remaining_vault.contains("credential:kerminal-plugin:other.notes:"));
    assert!(
        service
            .secret_get(&first, "shared", CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        service
            .state_get(&second, CancellationToken::new())
            .await
            .unwrap(),
        Some(json!({"plugin":"other.notes"}))
    );
    assert!(
        service
            .secret_get(&second, "shared", CancellationToken::new())
            .await
            .unwrap()
            .is_some()
    );
}

/// 用真实配置仓储模拟回执丢失，避免依赖平台特有的文件共享时序。
struct UnknownWrite {
    store: ConfigurationStore,
    commit: bool,
    writes: AtomicUsize,
}

#[async_trait::async_trait]
impl PluginSettingsRepository for UnknownWrite {
    /// 读回仍使用真实文件和 revision，确保恢复判断覆盖实际持久内容。
    async fn load(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> kerminal_application::AppResult<StoredPluginSettings> {
        PluginSettingsRepository::load(&self.store, plugin, cancellation).await
    }

    /// 按测试场景提交或不提交，随后统一模拟结果未知。
    async fn save(
        &self,
        plugin: &PluginId,
        values: Value,
        expected: Option<ConfigRevision>,
        cancellation: CancellationToken,
    ) -> kerminal_application::AppResult<ConfigRevision> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.commit {
            PluginSettingsRepository::save(&self.store, plugin, values, expected, cancellation)
                .await?;
        }
        Err(kerminal_application::AppError::outcome_unknown(
            "模拟写入回执丢失",
        ))
    }

    /// 此适配器不改变删除语义，保持测试范围集中在写入读回。
    async fn delete(
        &self,
        plugin: &PluginId,
        expected: Option<ConfigRevision>,
        cancellation: CancellationToken,
    ) -> kerminal_application::AppResult<Option<ConfigRevision>> {
        PluginSettingsRepository::delete(&self.store, plugin, expected, cancellation).await
    }
}

/// 只有回读的插件设置与目标完全一致才确认成功；两种结果均不得重放写入。
#[tokio::test]
async fn unknown_commit_requires_exact_readback_without_replay() {
    for committed in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let store = ConfigurationStore::initialize(root.path()).unwrap();
        let faulty = Arc::new(UnknownWrite {
            store: store.clone(),
            commit: committed,
            writes: AtomicUsize::new(0),
        });
        let service = PluginSettingsService::new(
            faulty.clone(),
            Arc::new(store.clone()),
            Arc::new(VaultStore::open(store.storage_coordinator())),
        );
        let plugin = PluginId::parse("acme.notes").unwrap();
        let schema = ConfigurationSchema::parse(
            br#"{"type":"object","properties":{"message":{"type":"string"}}}"#,
        )
        .unwrap();
        let result = service
            .save(
                &plugin,
                &schema,
                PluginSettingsSave {
                    values: json!({"message":"desired"}),
                    secrets: BTreeMap::new(),
                    expected_revision: None,
                },
                CancellationToken::new(),
            )
            .await;
        if committed {
            assert_eq!(result.unwrap().values, json!({"message":"desired"}));
        } else {
            assert_eq!(
                result.err().unwrap().public_code(),
                PublicErrorCode::OutcomeUnknown
            );
            assert!(!root.path().join("settings.toml").exists());
        }
        assert_eq!(faulty.writes.load(Ordering::SeqCst), 1);
    }
}

/// 用可控的未完成状态写入占住用例锁，验证排队操作仍能取消。
struct BlockedState {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl PluginStateRepository for BlockedState {
    /// 此测试只验证写入排队，不需要模拟持久读取。
    async fn get(
        &self,
        _: &PluginId,
        _: CancellationToken,
    ) -> kerminal_application::AppResult<Option<Value>> {
        Ok(None)
    }

    /// 通知测试锁已占用，并等待测试释放首个操作。
    async fn set(
        &self,
        _: &PluginId,
        _: Value,
        _: CancellationToken,
    ) -> kerminal_application::AppResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }

    /// 此测试不执行删除，提供明确的空实现以构造端口。
    async fn delete_data(
        &self,
        _: &PluginId,
        _: CancellationToken,
    ) -> kerminal_application::AppResult<()> {
        Ok(())
    }
}

/// 排队期间取消不能等待前一个慢操作结束，更不能执行后续状态写入。
#[tokio::test]
async fn queued_write_can_be_cancelled_without_waiting_for_current_write() {
    let root = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::initialize(root.path()).unwrap();
    let blocked = Arc::new(BlockedState {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let service = PluginSettingsService::new(
        Arc::new(store.clone()),
        blocked.clone(),
        Arc::new(VaultStore::open(store.storage_coordinator())),
    );
    let plugin = PluginId::parse("acme.notes").unwrap();
    let first = {
        let service = service.clone();
        let plugin = plugin.clone();
        tokio::spawn(async move {
            service
                .state_set(&plugin, json!(1), CancellationToken::new())
                .await
        })
    };
    blocked.entered.notified().await;
    let cancellation = CancellationToken::new();
    let second = {
        let service = service.clone();
        let plugin = plugin.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move { service.state_set(&plugin, json!(2), cancellation).await })
    };
    cancellation.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), second)
        .await
        .expect("取消必须及时返回")
        .unwrap();
    assert_eq!(
        result.unwrap_err().public_code(),
        PublicErrorCode::Cancelled
    );
    blocked.release.notify_one();
    first.await.unwrap().unwrap();
}

/// 本地锚点与 patternProperties 的秘密注解同样必须经过 Vault，不能落盘为明文。
#[tokio::test]
async fn schema_anchors_and_pattern_properties_protect_secrets() {
    let root = tempfile::tempdir().unwrap();
    let (_store, service) = fixture(root.path());
    let plugin = PluginId::parse("acme.notes").unwrap();
    let schema = ConfigurationSchema::parse(
        br##"{
      "$schema":"https://json-schema.org/draft/2020-12/schema",
      "type":"object",
      "properties":{"token":{"$ref":"#credentials"}},
      "patternProperties":{"^extra-":{"type":"string","x-secret":true}},
      "$defs":{"credentials":{"$anchor":"credentials","type":"string","x-secret":true}}
    }"##,
    )
    .unwrap();
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "/token".into(),
        PluginSecretEdit::Replace(Zeroizing::new("anchor-secret".into())),
    );
    secrets.insert(
        "/extra-key".into(),
        PluginSecretEdit::Replace(Zeroizing::new("pattern-secret".into())),
    );
    let snapshot = service
        .save(
            &plugin,
            &schema,
            PluginSettingsSave {
                values: json!({"token":null,"extra-key":null}),
                secrets,
                expected_revision: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(snapshot.secret_paths.len(), 2);
    assert_eq!(
        service
            .load(&plugin, &schema, CancellationToken::new())
            .await
            .unwrap(),
        snapshot
    );
    let source = fs::read_to_string(root.path().join("settings.toml")).unwrap();
    assert!(!source.contains("anchor-secret"));
    assert!(!source.contains("pattern-secret"));
}

/// 丢失 key 时不能把受影响 Vault 的未知槽误报为空，也不能清除仍可恢复的密文。
#[tokio::test]
async fn missing_vault_key_preserves_encrypted_material() {
    let root = tempfile::tempdir().unwrap();
    let (_store, service) = fixture(root.path());
    let plugin = PluginId::parse("acme.notes").unwrap();
    service
        .secret_set(
            &plugin,
            "saved",
            Zeroizing::new("fixture".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let vault_path = root.path().join("secrets/vault.toml");
    let before = fs::read(&vault_path).unwrap();
    fs::remove_file(root.path().join("secrets/vault-key.toml")).unwrap();
    assert!(
        service
            .secret_get(&plugin, "missing", CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        service
            .delete_data(&plugin, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(fs::read(vault_path).unwrap(), before);
}
