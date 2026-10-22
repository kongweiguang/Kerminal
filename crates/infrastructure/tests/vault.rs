//! 凭据 vault 与 schema 1 key 文档的隔离集成测试。
//!
//! 所有路径均使用临时目录，fixture 中的口令和 key 是人工测试数据。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use kerminal_application::{
    connections::credentials::{
        CredentialPresence, CredentialService, SaveCredentialRequest, SecretValue,
        VaultKeyAvailability, VaultKeyDocument, VaultKeyExportMode, VaultKeyManagementService,
        VaultRecoveryReason,
    },
    connections::{CredentialResolver, ResolvedCredential},
};
use kerminal_domain::{
    connections::{
        CredentialRef,
        credentials::{CredentialKind, CredentialMaterial, CredentialScope},
    },
    identity::HostId,
};
use kerminal_infrastructure::{storage::StorageCoordinator, vault::VaultStore};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const FIXTURE_KEY: &[u8] = include_bytes!("../../../tests/fixtures/vault/vault-key-v1.toml");
const FIXTURE_VAULT: &[u8] = include_bytes!("../../../tests/fixtures/vault/vault-v1.toml");
const FIXTURE_SECRET: &str = "fixture-only synthetic password";
const FIXTURE_REFERENCE: &str = "credential:kerminal:ssh-host:fixture-host:target:password:v1";
const FROZEN_FIXTURE_ASSOCIATED_DATA: &str =
    "Y3JlZGVudGlhbDprZXJtaW5hbDpzc2gtaG9zdDpmaXh0dXJlLWhvc3Q6dGFyZ2V0OnBhc3N3b3JkOnYx";
const FROZEN_FIXTURE_NONCE: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYX";
const FROZEN_FIXTURE_CIPHERTEXT: &str =
    "+Kt3C+Wg6INcKkq36yHRhj8vTdGUtT2bHmlMwVLfkQapkVTJIhvcaJnQsG5mn/A=";

/// 只在兼容测试中固定 v0.3.45 key 字段，避免新旧实现共享同一解析路径。
#[derive(Deserialize)]
struct FrozenLegacyVaultKey {
    schema_version: u32,
    key_id: String,
    algorithm: String,
    created_at: String,
    master_key: String,
}

/// 只在兼容测试中固定 v0.3.45 vault 根对象字段。
#[derive(Deserialize)]
struct FrozenLegacyVault {
    schema_version: u32,
    entries: Vec<FrozenLegacyVaultEntry>,
}

/// 只在兼容测试中固定旧格式 entry 字段，不为测试正文派生 Debug。
#[derive(Deserialize)]
struct FrozenLegacyVaultEntry {
    id: String,
    kind: String,
    algorithm: String,
    key_id: String,
    associated_data: String,
    nonce: String,
    ciphertext: String,
    created_at: String,
    updated_at: String,
}

/// 固定旧版 key、nonce、AAD 与密文，并区分引用 namespace 与实际 credential kind。
///
/// @author kongweiguang
#[test]
fn v0_3_45_fixture_has_frozen_xchacha_ciphertext_bytes() {
    let key_source = std::str::from_utf8(FIXTURE_KEY).expect("fixture key is UTF-8");
    let key: FrozenLegacyVaultKey = toml::from_str(key_source).expect("parse frozen key schema");
    assert_eq!(key.schema_version, 1);
    assert_eq!(key.key_id, "workspace-default");
    assert_eq!(key.algorithm, "xchacha20poly1305");
    assert!(!key.created_at.is_empty());
    let key_bytes = Zeroizing::new(
        BASE64
            .decode(&key.master_key)
            .expect("decode synthetic fixture master key"),
    );
    assert_eq!(key_bytes.len(), 32, "legacy key schema requires 32 bytes");

    let vault_source = std::str::from_utf8(FIXTURE_VAULT).expect("fixture vault is UTF-8");
    let vault: FrozenLegacyVault = toml::from_str(vault_source).expect("parse frozen vault schema");
    assert_eq!(vault.schema_version, 1);
    let entry = vault
        .entries
        .iter()
        .find(|entry| entry.id == FIXTURE_REFERENCE)
        .expect("find frozen credential entry");
    assert!(FIXTURE_REFERENCE.starts_with("credential:kerminal:ssh-host:"));
    assert_eq!(entry.kind, "ssh-password");
    assert_eq!(entry.algorithm, "xchacha20poly1305");
    assert_eq!(entry.key_id, key.key_id);
    assert!(entry.associated_data == FROZEN_FIXTURE_ASSOCIATED_DATA);
    assert!(entry.nonce == FROZEN_FIXTURE_NONCE);
    assert!(entry.ciphertext == FROZEN_FIXTURE_CIPHERTEXT);
}

/// 为每个集成测试创建互不共享的配置根，避免接触用户真实凭据。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 用随机子目录隔离并发测试，测试结束后只清理本测试创建的路径。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-vault-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("create isolated test root");
        Self(path)
    }

    /// 向 adapter 提供测试专属 root 的只读路径。
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    /// 仅删除随机测试目录，绝不从环境变量或用户配置路径推导清理目标。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 把静态旧格式 fixture 复制到临时用户数据根，不复用或读取默认配置目录。
fn install_legacy_pair(root: &Path, include_key: bool) {
    let secrets = root.join("secrets");
    fs::create_dir_all(&secrets).expect("create isolated secrets directory");
    if include_key {
        fs::write(secrets.join("vault-key.toml"), FIXTURE_KEY).expect("write fixture key");
    }
    fs::write(secrets.join("vault.toml"), FIXTURE_VAULT).expect("write fixture vault");
}

/// 构造与 legacy fixture 完全一致的 opaque ref，测试不需在外部复制秘密字段。
fn fixture_reference() -> CredentialRef {
    CredentialRef::new(FIXTURE_REFERENCE.to_owned()).expect("valid fixture reference")
}

/// 验证冻结的 schema 1 key/vault 可以解密旧条目，并经应用凭据用例增删新秘密。
#[tokio::test]
async fn legacy_fixture_resolves_and_service_round_trips_without_secret_leakage() {
    let root = TestRoot::new();
    install_legacy_pair(root.path(), true);
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let credentials = CredentialService::new(Arc::new(store.clone()));
    let reference = fixture_reference();

    assert_eq!(
        credentials.inspect(&reference).await.expect("inspect"),
        CredentialPresence::Saved
    );
    let resolved = store
        .resolve(&reference, &CancellationToken::new())
        .await
        .expect("resolve old-format entry");
    assert!(
        matches!(resolved, ResolvedCredential::Password(value) if value.as_str() == FIXTURE_SECRET)
    );

    let saved = credentials
        .save(SaveCredentialRequest {
            kind: CredentialKind::parse("ssh-host").expect("kind"),
            host_id: HostId::parse("new-host").expect("host id"),
            scope: CredentialScope::target(),
            material: CredentialMaterial::Password,
            secret: SecretValue::new("temporary manual test secret".to_owned()),
        })
        .await
        .expect("save credential through use case");
    let resolved = store
        .resolve(&saved, &CancellationToken::new())
        .await
        .expect("resolve new entry");
    assert!(
        matches!(resolved, ResolvedCredential::Password(value) if value.as_str() == "temporary manual test secret")
    );

    let vault_text =
        fs::read_to_string(root.path().join("secrets/vault.toml")).expect("read ciphertext");
    assert!(!vault_text.contains(FIXTURE_SECRET));
    assert!(!vault_text.contains("temporary manual test secret"));
    assert!(!format!("{:?}", SecretValue::new(FIXTURE_SECRET.to_owned())).contains(FIXTURE_SECRET));
    assert!(
        !format!(
            "{:?}",
            VaultKeyDocument::new(String::from_utf8_lossy(FIXTURE_KEY).into_owned())
        )
        .contains("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=")
    );

    assert!(
        credentials
            .remove(&saved)
            .await
            .expect("remove saved credential")
    );
    assert_eq!(
        credentials
            .inspect(&saved)
            .await
            .expect("inspect removed credential"),
        CredentialPresence::Missing
    );
}

/// 验证新建 key/vault 继续输出冻结 v0.3.45 schema、AAD、nonce 与 AEAD tag 长度。
#[tokio::test]
async fn fresh_pair_preserves_v0_3_45_schema_and_wire_lengths() {
    let root = TestRoot::new();
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let credentials = CredentialService::new(Arc::new(store));
    let saved = credentials
        .save(SaveCredentialRequest {
            kind: CredentialKind::parse("ssh-host").expect("kind"),
            host_id: HostId::parse("fresh-host").expect("host id"),
            scope: CredentialScope::target(),
            material: CredentialMaterial::Password,
            secret: SecretValue::new("fresh-pair manual test secret".to_owned()),
        })
        .await
        .expect("create schema 1 pair");

    let key_source =
        fs::read_to_string(root.path().join("secrets/vault-key.toml")).expect("read test key");
    let vault_source =
        fs::read_to_string(root.path().join("secrets/vault.toml")).expect("read test vault");
    let key: FrozenLegacyVaultKey = toml::from_str(&key_source).expect("parse legacy key schema");
    let vault: FrozenLegacyVault =
        toml::from_str(&vault_source).expect("parse legacy vault schema");
    assert_eq!(key.schema_version, 1);
    assert_eq!(key.algorithm, "xchacha20poly1305");
    assert_eq!(vault.schema_version, 1);
    let entry = vault
        .entries
        .iter()
        .find(|entry| entry.id == saved.as_str())
        .expect("find saved credential entry");
    assert_eq!(entry.algorithm, key.algorithm);
    assert_eq!(entry.key_id, key.key_id);
    assert!(!entry.created_at.is_empty());
    assert!(!entry.updated_at.is_empty());

    let master_key = Zeroizing::new(
        BASE64
            .decode(&key.master_key)
            .expect("decode legacy key encoding"),
    );
    assert_eq!(master_key.len(), 32);
    let nonce = BASE64.decode(&entry.nonce).expect("decode legacy nonce");
    assert_eq!(nonce.len(), 24);
    let aad = BASE64
        .decode(&entry.associated_data)
        .expect("decode legacy associated data");
    assert_eq!(aad, saved.as_str().as_bytes());
    let ciphertext = BASE64
        .decode(&entry.ciphertext)
        .expect("decode legacy ciphertext");
    assert_eq!(ciphertext.len(), "fresh-pair manual test secret".len() + 16);
}

/// 验证当前 resolver 对缺失 AAD fail closed，旧数据必须先经过显式迁移。
#[tokio::test]
async fn current_resolver_rejects_entry_without_saved_aad() {
    let root = TestRoot::new();
    let secrets = root.path().join("secrets");
    fs::create_dir_all(&secrets).expect("create isolated secrets directory");
    fs::write(secrets.join("vault-key.toml"), FIXTURE_KEY).expect("write synthetic key");
    let original = std::str::from_utf8(FIXTURE_VAULT).expect("fixture is UTF-8");
    let encoded_aad = BASE64.encode(FIXTURE_REFERENCE.as_bytes());
    let source_aad = format!("associated_data = \"{encoded_aad}\"");
    let vault_without_aad = original.replace(&source_aad, "associated_data = \"\"");
    assert_ne!(
        vault_without_aad, original,
        "fixture AAD replacement must apply"
    );
    fs::write(secrets.join("vault.toml"), vault_without_aad)
        .expect("write synthetic legacy entry without stored AAD");

    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let result = store
        .resolve(&fixture_reference(), &CancellationToken::new())
        .await;
    assert!(
        result.is_err(),
        "current reader must require stored AAD metadata"
    );
    if let Err(error) = result {
        assert!(!error.to_string().contains(FIXTURE_REFERENCE));
    }
}

/// 验证同步到新设备的密文缺少本机 key 时仍可启动并导入匹配 key 恢复访问。
#[tokio::test]
async fn ciphertext_only_sync_state_can_import_its_matching_key() {
    let root = TestRoot::new();
    install_legacy_pair(root.path(), false);
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let references = CredentialService::new(Arc::new(store.clone()));
    let keys = VaultKeyManagementService::new(Arc::new(store.clone()));
    let reference = fixture_reference();

    assert_eq!(
        keys.ensure().await.expect("check vault bootstrap"),
        VaultKeyAvailability::MissingForExistingVault
    );
    assert_eq!(
        references
            .inspect(&reference)
            .await
            .expect("inspect ciphertext"),
        CredentialPresence::Saved
    );
    assert!(
        store
            .resolve(&reference, &CancellationToken::new())
            .await
            .is_err()
    );

    let ciphertext_before =
        fs::read(root.path().join("secrets/vault.toml")).expect("read synced ciphertext");
    let receipt = keys
        .import(
            VaultKeyDocument::new(String::from_utf8_lossy(FIXTURE_KEY).into_owned()),
            false,
        )
        .await
        .expect("import paired key");
    assert!(!receipt.dry_run);
    assert_eq!(receipt.entry_count, 1);
    assert_eq!(
        fs::read(root.path().join("secrets/vault.toml")).expect("read vault after import"),
        ciphertext_before
    );
    let resolved = store
        .resolve(&reference, &CancellationToken::new())
        .await
        .expect("resolve after key import");
    assert!(
        matches!(resolved, ResolvedCredential::Password(value) if value.as_str() == FIXTURE_SECRET)
    );
}

/// 验证错误 key 导入不覆盖 pair，dry-run 不落盘且真实轮换保留旧凭据可读。
#[tokio::test]
async fn invalid_import_and_rotation_preserve_recoverable_pair() {
    let root = TestRoot::new();
    install_legacy_pair(root.path(), true);
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let keys = VaultKeyManagementService::new(Arc::new(store.clone()));
    let reference = fixture_reference();
    let key_path = root.path().join("secrets/vault-key.toml");
    let vault_path = root.path().join("secrets/vault.toml");
    let original_key = fs::read(&key_path).expect("read original key");
    let original_vault = fs::read(&vault_path).expect("read original vault");

    let invalid_key = "schema_version = 1\nkey_id = 'wrong-key'\nalgorithm = 'xchacha20poly1305'\ncreated_at = '1700000000'\nmaster_key = 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='\n";
    assert!(
        keys.import(VaultKeyDocument::new(invalid_key.to_owned()), false)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(&key_path).expect("key after rejected import"),
        original_key
    );
    assert_eq!(
        fs::read(&vault_path).expect("vault after rejected import"),
        original_vault
    );

    let mismatched_master_key = "schema_version = 1\nkey_id = \"workspace-default\"\nalgorithm = \"xchacha20poly1305\"\ncreated_at = \"1700000000\"\nmaster_key = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"\n";
    assert!(
        keys.import(
            VaultKeyDocument::new(mismatched_master_key.to_owned()),
            false
        )
        .await
        .is_err(),
        "a key with matching metadata but wrong key bytes must not replace the pair"
    );
    assert_eq!(
        fs::read(&key_path).expect("key after mismatched master-key import"),
        original_key
    );
    assert_eq!(
        fs::read(&vault_path).expect("vault after mismatched master-key import"),
        original_vault
    );

    let dry_run = keys.rotate(true).await.expect("dry-run rotation");
    assert!(dry_run.dry_run);
    assert_eq!(
        fs::read(&key_path).expect("key after dry-run"),
        original_key
    );
    assert_eq!(
        fs::read(&vault_path).expect("vault after dry-run"),
        original_vault
    );

    let rotated = keys.rotate(false).await.expect("rotate key and vault");
    assert!(rotated.backup_created);
    assert_eq!(rotated.entry_count, 1);
    assert_ne!(fs::read(&key_path).expect("rotated key"), original_key);
    assert_ne!(
        fs::read(&vault_path).expect("rotated vault"),
        original_vault
    );
    assert!(
        store
            .resolve(&reference, &CancellationToken::new())
            .await
            .is_ok()
    );
    let backups = fs::read_dir(root.path().join("secrets"))
        .expect("read backup directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak."))
        .count();
    assert!(backups >= 2);
}

/// 验证进程中断留下 active journal 时，首次 key readiness 查询先恢复完整旧 pair。
#[tokio::test]
async fn active_pair_journal_restores_both_files_before_readiness() {
    let root = TestRoot::new();
    install_legacy_pair(root.path(), true);
    let secrets = root.path().join("secrets");
    let journal = secrets.join(".vault-transaction/active");
    fs::create_dir_all(&journal).expect("create crash-recovery journal");
    fs::write(journal.join("manifest"), [1_u8, 1_u8]).expect("write journal manifest");
    fs::write(journal.join("key.backup"), FIXTURE_KEY).expect("write old key snapshot");
    fs::write(journal.join("vault.backup"), FIXTURE_VAULT).expect("write old vault snapshot");
    fs::write(secrets.join("vault-key.toml"), b"incomplete key write").expect("simulate key crash");
    fs::write(secrets.join("vault.toml"), b"incomplete vault write").expect("simulate vault crash");

    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let keys = VaultKeyManagementService::new(Arc::new(store.clone()));
    assert!(matches!(
        keys.availability()
            .await
            .expect("recover pair during readiness"),
        VaultKeyAvailability::Ready { .. }
    ));
    assert_eq!(
        fs::read(secrets.join("vault-key.toml")).expect("restored key"),
        FIXTURE_KEY
    );
    assert_eq!(
        fs::read(secrets.join("vault.toml")).expect("restored vault"),
        FIXTURE_VAULT
    );
    assert!(!journal.exists());
    assert!(
        store
            .resolve(&fixture_reference(), &CancellationToken::new())
            .await
            .is_ok()
    );
}

/// 验证 Git sync 获取共享锁时会拒绝无法恢复的半提交 pair，不读取或覆盖原 vault。
#[test]
fn sync_lock_rejects_unrecoverable_pair_journal() {
    let root = TestRoot::new();
    install_legacy_pair(root.path(), true);
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let vault_path = root.path().join("secrets/vault.toml");
    let original_vault = fs::read(&vault_path).expect("read original fixture vault");
    let active = root.path().join("secrets/.vault-transaction/active");
    fs::create_dir_all(&active).expect("create interrupted transaction marker");
    fs::write(active.join("manifest"), [2_u8, 1_u8]).expect("write invalid marker fixture");

    assert!(matches!(
        store.lock_for_sync(),
        Err(kerminal_infrastructure::vault::VaultError::RecoveryRequired)
    ));
    assert_eq!(
        fs::read(&vault_path).expect("read vault after rejected sync"),
        original_vault
    );
    assert!(active.exists(), "recovery evidence must remain available");
}

/// 验证损坏 pair 仍可打开主应用、显示 typed 原因，显式恢复后读取旧 fixture。
#[tokio::test]
async fn corrupt_pair_is_visible_until_explicit_validated_restore() {
    let root = TestRoot::new();
    let secrets = root.path().join("secrets");
    fs::create_dir_all(&secrets).expect("create isolated secrets directory");
    let corrupt_key = b"not a key document";
    let corrupt_vault = b"schema_version = 99\nentries = []\n";
    fs::write(secrets.join("vault-key.toml"), corrupt_key).expect("write corrupt key");
    fs::write(secrets.join("vault.toml"), corrupt_vault).expect("write corrupt vault");
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let keys = VaultKeyManagementService::new(Arc::new(store.clone()));
    let credentials = CredentialService::new(Arc::new(store.clone()));
    assert_eq!(
        keys.availability().await.expect("read recovery state"),
        VaultKeyAvailability::RecoveryRequired {
            reason: VaultRecoveryReason::InvalidKey,
        }
    );
    assert!(
        credentials
            .save(SaveCredentialRequest {
                kind: CredentialKind::parse("ssh-host").expect("kind"),
                host_id: HostId::parse("blocked-host").expect("host id"),
                scope: CredentialScope::target(),
                material: CredentialMaterial::Password,
                secret: SecretValue::new("must not replace broken pair".to_owned()),
            })
            .await
            .is_err(),
        "credential writes stay fail-closed while the pair is damaged"
    );
    let selected_key = root.path().join("selected-vault-key.toml");
    let selected_vault = root.path().join("selected-vault.toml");
    let invalid_selected_key = root.path().join("invalid-selected-key.toml");
    fs::write(&invalid_selected_key, b"not a vault key").expect("write invalid selected key");
    assert!(
        keys.restore_pair_from_paths(invalid_selected_key, selected_vault.clone(), false,)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(secrets.join("vault-key.toml")).expect("key after rejected restore"),
        corrupt_key
    );
    assert_eq!(
        fs::read(secrets.join("vault.toml")).expect("vault after rejected restore"),
        corrupt_vault
    );
    fs::write(&selected_key, FIXTURE_KEY).expect("write selected synthetic key");
    fs::write(&selected_vault, FIXTURE_VAULT).expect("write selected synthetic vault");
    let receipt = keys
        .restore_pair_from_paths(selected_key, selected_vault, false)
        .await
        .expect("restore fully validated pair");
    assert!(receipt.backup_created);
    assert_eq!(receipt.entry_count, 1);
    assert!(matches!(
        keys.availability().await.expect("read recovered state"),
        VaultKeyAvailability::Ready { .. }
    ));
    assert!(matches!(
        store
            .resolve(&fixture_reference(), &CancellationToken::new())
            .await
            .expect("read restored fixture credential"),
        ResolvedCredential::Password(value) if value.as_str() == FIXTURE_SECRET
    ));
    let key_backup = fs::read_dir(&secrets)
        .expect("list preserved key backup")
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("vault-key.toml.bak.")
        })
        .expect("original key backup exists");
    assert_eq!(
        fs::read(key_backup.path()).expect("read key backup"),
        corrupt_key
    );
    let vault_backup = fs::read_dir(&secrets)
        .expect("list preserved vault backup")
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("vault.toml.bak.")
        })
        .expect("original vault backup exists");
    assert_eq!(
        fs::read(vault_backup.path()).expect("read vault backup"),
        corrupt_vault
    );
}

/// 分别拒绝未知 key/vault schema，并确保只读诊断与失败读取保留原文件字节。
#[tokio::test]
async fn future_key_and_vault_schemas_are_visible_and_preserved() {
    let future_key = std::str::from_utf8(FIXTURE_KEY)
        .expect("key fixture is UTF-8")
        .replace("schema_version = 1", "schema_version = 2");
    let future_vault = std::str::from_utf8(FIXTURE_VAULT)
        .expect("vault fixture is UTF-8")
        .replace("schema_version = 1", "schema_version = 2");
    let cases = [
        (
            "future key schema",
            future_key.as_bytes(),
            FIXTURE_VAULT,
            VaultRecoveryReason::InvalidKey,
        ),
        (
            "future vault schema",
            FIXTURE_KEY,
            future_vault.as_bytes(),
            VaultRecoveryReason::InvalidVault,
        ),
    ];

    for (label, key_bytes, vault_bytes, expected_reason) in cases {
        let root = TestRoot::new();
        let secrets = root.path().join("secrets");
        fs::create_dir_all(&secrets).expect("create isolated secrets directory");
        let key_path = secrets.join("vault-key.toml");
        let vault_path = secrets.join("vault.toml");
        fs::write(&key_path, key_bytes).expect("write future-schema key fixture");
        fs::write(&vault_path, vault_bytes).expect("write future-schema vault fixture");
        let key_before = fs::read(&key_path).expect("read original future key");
        let vault_before = fs::read(&vault_path).expect("read original future vault");

        let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
        let keys = VaultKeyManagementService::new(Arc::new(store.clone()));
        assert_eq!(
            keys.availability().await.expect("read recovery status"),
            VaultKeyAvailability::RecoveryRequired {
                reason: expected_reason,
            },
            "{label} must be visible as a recovery state"
        );
        assert!(
            store
                .resolve(&fixture_reference(), &CancellationToken::new())
                .await
                .is_err(),
            "{label} must not produce a credential"
        );
        assert_eq!(
            fs::read(&key_path).expect("read key after rejected access"),
            key_before,
            "{label} must preserve the key file"
        );
        assert_eq!(
            fs::read(&vault_path).expect("read vault after rejected access"),
            vault_before,
            "{label} must preserve the vault file"
        );
    }
}

/// 验证 key 文件导入支持验证预演，导出需显式选择新建或覆盖并使用私有原子文件。
#[tokio::test]
async fn selected_key_file_import_and_export_use_explicit_path_modes() {
    let root = TestRoot::new();
    install_legacy_pair(root.path(), true);
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let keys = VaultKeyManagementService::new(Arc::new(store));
    let key_path = root.path().join("selected-key.toml");
    let current_key_path = root.path().join("secrets/vault-key.toml");
    let current_key_before = fs::read(&current_key_path).expect("read original key");
    let vault_before =
        fs::read(root.path().join("secrets/vault.toml")).expect("read existing encrypted vault");
    fs::write(&key_path, FIXTURE_KEY).expect("write selected synthetic key");

    let preview = keys
        .import_from_path(key_path.clone(), true)
        .await
        .expect("validate selected key without committing");
    assert!(preview.dry_run);
    assert_eq!(
        fs::read(&current_key_path).expect("key after preview"),
        current_key_before
    );
    assert_eq!(
        fs::read(root.path().join("secrets/vault.toml")).expect("vault after preview"),
        vault_before
    );

    let imported = keys
        .import_from_path(key_path, false)
        .await
        .expect("import matching selected key");
    assert!(!imported.dry_run);
    assert!(imported.backup_created);

    let expected = keys
        .export()
        .await
        .expect("read normalized export document");
    let export_directory = root.path().join("user-selected-export");
    fs::create_dir_all(&export_directory).expect("create selected destination directory");
    let export_path = export_directory.join("vault-key.toml");
    keys.export_to_path(export_path.clone(), VaultKeyExportMode::CreateNew)
        .await
        .expect("export to new selected file");
    let exported = fs::read(&export_path).expect("read selected export");
    assert_eq!(exported, expected.as_str().as_bytes());

    let conflict = keys
        .export_to_path(export_path.clone(), VaultKeyExportMode::CreateNew)
        .await
        .expect_err("create-new must reject an existing export");
    assert!(!format!("{conflict:?}").contains("master_key"));
    assert_eq!(
        fs::read(&export_path).expect("read export after conflict"),
        exported
    );

    keys.export_to_path(export_path.clone(), VaultKeyExportMode::ReplaceExisting)
        .await
        .expect("explicitly replace selected export");
    assert_eq!(
        fs::read(&export_path).expect("read replaced export"),
        expected.as_str().as_bytes()
    );
    let absent_path = export_directory.join("missing-key.toml");
    assert!(
        keys.export_to_path(absent_path, VaultKeyExportMode::ReplaceExisting)
            .await
            .is_err()
    );
}

/// 验证 Unix key、密文和目录权限维持 owner-only，Windows 由 profile ACL 继承。
#[cfg(unix)]
#[test]
fn vault_files_have_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestRoot::new();
    install_legacy_pair(root.path(), true);
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create isolated runtime");
    assert!(matches!(
        runtime.block_on(VaultKeyManagementService::new(Arc::new(store)).availability()),
        Ok(VaultKeyAvailability::Ready { .. })
    ));

    assert_eq!(
        fs::metadata(root.path().join("secrets"))
            .expect("secrets metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for file in ["vault-key.toml", "vault.toml"] {
        assert_eq!(
            fs::metadata(root.path().join("secrets").join(file))
                .expect("vault file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
