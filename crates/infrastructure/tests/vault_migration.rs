//! 当前 schema 1 vault 的一次性 AAD 迁移隔离测试。
//!
//! @author kongweiguang

use std::{fs, path::Path};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use kerminal_application::{
    connections::{CredentialResolver, ResolvedCredential},
    data_migration::{
        DataMigrationBlocker, DataMigrationOutcome, DataMigrationReadiness, DataMigrationRepository,
    },
};
use kerminal_domain::connections::CredentialRef;
use kerminal_infrastructure::{
    data_migration::FileDataMigrationRepository, storage::StorageCoordinator, vault::VaultStore,
};
use tokio_util::sync::CancellationToken;

const FIXTURE_KEY: &[u8] = include_bytes!("../../../tests/fixtures/vault/vault-key-v1.toml");
const FIXTURE_VAULT: &[u8] = include_bytes!("../../../tests/fixtures/vault/vault-v1.toml");
const FIXTURE_REFERENCE: &str = "credential:kerminal:ssh-host:fixture-host:target:password:v1";
const FIXTURE_SECRET: &str = "fixture-only synthetic password";

/// 创建仅含人工加密 fixture 的根目录，避免读取真实用户 key 或 vault。
fn install_vault(root: &Path, vault: &str) {
    let secrets = root.join("secrets");
    fs::create_dir_all(&secrets).expect("创建隔离的 secrets 目录");
    fs::write(secrets.join("vault.toml"), vault).expect("写入人工加密 fixture");
}

/// 将 fixture 变成旧 writer 可能留下的空 AAD，同时加入未知字段与注释保护样本。
fn fixture_with_empty_aad_and_user_data() -> String {
    let source = std::str::from_utf8(FIXTURE_VAULT).expect("fixture 为 UTF-8");
    let aad = BASE64.encode(FIXTURE_REFERENCE.as_bytes());
    let source = source
        .replace("[[entries]]\n", "# 用户注释应保留\n[[entries]]\n")
        .replace(
            &format!("associated_data = \"{aad}\"\n"),
            "associated_data = \"\" # AAD 注释应保留\n",
        )
        .replace(
            "updated_at = \"1700000000\"\n",
            "updated_at = \"1700000000\"\ncustom_note = \"保留未知字段\"\n",
        );
    assert!(source.contains("associated_data = \"\" # AAD 注释应保留"));
    assert!(source.contains("custom_note = \"保留未知字段\""));
    source
}

/// 生成三条达到单凭据 writer 上限的结构化密文，专门覆盖聚合文件超过 16 MiB 的迁移路径。
///
/// @author kongweiguang
fn large_synthetic_vault() -> (String, String) {
    let source = std::str::from_utf8(FIXTURE_VAULT).expect("fixture 为 UTF-8");
    let entry = source
        .split_once("[[entries]]\n")
        .map(|(_, entry)| entry)
        .expect("fixture 含 schema 1 条目");
    let fixture_aad = BASE64.encode(FIXTURE_REFERENCE.as_bytes());
    let fixture_ciphertext = "+Kt3C+Wg6INcKkq36yHRhj8vTdGUtT2bHmlMwVLfkQapkVTJIhvcaJnQsG5mn/A=";
    // writer 的单项 plaintext 上限为 4 MiB，XChaCha20-Poly1305 再附加 16 字节认证标签。
    let encoded_ciphertext = BASE64.encode(vec![0_u8; 4 * 1024 * 1024 + 16]);
    let entries = ["target", "jump-0", "jump-1"]
        .map(|scope| {
            let reference =
                format!("credential:kerminal:ssh-host:fixture-host:{scope}:password:v1");
            format!(
                "[[entries]]\n{}",
                entry
                    .replace(FIXTURE_REFERENCE, &reference)
                    .replace(
                        &format!("associated_data = \"{fixture_aad}\""),
                        "associated_data = \"\"",
                    )
                    .replace(
                        &format!("ciphertext = \"{fixture_ciphertext}\""),
                        &format!("ciphertext = \"{encoded_ciphertext}\""),
                    )
            )
        })
        .join("\n");
    (
        format!("schema_version = 1\n\n{entries}"),
        encoded_ciphertext,
    )
}

/// 校验空 AAD 只补等价编码、无需 key、干跑无写入且共用事务可备份和幂等读回。
#[tokio::test]
async fn missing_aad_migrates_without_key_and_preserves_ciphertext_bytes() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let source = fixture_with_empty_aad_and_user_data();
    install_vault(root.path(), &source);
    let vault_path = root.path().join("secrets/vault.toml");
    let repository = FileDataMigrationRepository::new(root.path());
    let original_bytes = fs::read(&vault_path).expect("读取迁移前 vault");

    let dry_run = repository.inspect().expect("只读预检成功");
    assert_eq!(dry_run.readiness, DataMigrationReadiness::Legacy);
    assert_eq!(
        fs::read(&vault_path).expect("干跑后 vault 仍可读"),
        original_bytes
    );
    assert!(!root.path().join(".kerminal-format.toml").exists());
    assert!(!root.path().join(".kerminal-migration").exists());

    let report = repository.migrate().expect("没有 key 也可迁移 AAD 元数据");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    assert!(report.changed_file_count >= 1);
    let operation = report
        .backup_relative_path
        .as_deref()
        .expect("迁移返回完整恢复目录");
    let backup = root
        .path()
        .join(operation)
        .join("before/managed/secrets/vault.toml");
    assert_eq!(
        fs::read(backup).expect("vault 原始备份保留"),
        original_bytes
    );

    let migrated = fs::read_to_string(&vault_path).expect("读取迁移后 vault");
    let aad = BASE64.encode(FIXTURE_REFERENCE.as_bytes());
    assert_eq!(
        migrated,
        source.replace(
            "associated_data = \"\" # AAD 注释应保留",
            &format!("associated_data = \"{aad}\" # AAD 注释应保留"),
        ),
        "只应替换 AAD 值并保留其它字节、注释和未知字段"
    );
    assert!(migrated.contains("# 用户注释应保留"));
    assert!(migrated.contains("custom_note = \"保留未知字段\""));
    assert!(!root.path().join("secrets/vault-key.toml").exists());

    let key_path = root.path().join("secrets/vault-key.toml");
    fs::write(&key_path, FIXTURE_KEY).expect("恢复原始 fixture key");
    assert_eq!(
        fs::read(&key_path).expect("回读原始 fixture key"),
        FIXTURE_KEY
    );
    let store = VaultStore::open(StorageCoordinator::new(root.path().to_path_buf()));
    let reference = CredentialRef::new(FIXTURE_REFERENCE.to_owned()).expect("fixture ref 有效");
    let resolved = store
        .resolve(&reference, &CancellationToken::new())
        .await
        .expect("迁移后原 key 可解密原密文");
    assert!(matches!(
        resolved,
        ResolvedCredential::Password(value) if value.as_str() == FIXTURE_SECRET
    ));

    let before_idempotent_readback = fs::read(&vault_path).expect("保存幂等前字节");
    let second = repository.migrate().expect("重复迁移安全读回");
    assert_eq!(second.outcome, DataMigrationOutcome::AlreadyCurrent);
    assert_eq!(
        fs::read(&vault_path).expect("幂等后 vault 可读"),
        before_idempotent_readback
    );
}

/// 确认 planner 与共享事务可迁移单文件大于 16 MiB 的 schema 1 vault，不增加 writer 总量上限。
///
/// @author kongweiguang
#[test]
fn aggregate_vault_above_sixteen_mib_migrates_without_changing_ciphertext() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let (source, ciphertext) = large_synthetic_vault();
    let limit = 16 * 1024 * 1024;
    assert!(source.len() > limit, "样本必须越过旧 planner 上限");
    install_vault(root.path(), &source);
    let vault_path = root.path().join("secrets/vault.toml");
    let original_bytes = fs::read(&vault_path).expect("读取大 vault 原文");
    assert!(original_bytes.len() > limit);

    let repository = FileDataMigrationRepository::new(root.path());
    assert_eq!(
        repository
            .inspect()
            .expect("大 vault 只读预检成功")
            .readiness,
        DataMigrationReadiness::Legacy
    );
    assert_eq!(
        fs::read(&vault_path).expect("预检不写入 vault"),
        original_bytes
    );
    assert!(!root.path().join(".kerminal-migration").exists());

    let report = repository.migrate().expect("大 vault 通过共享事务迁移");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    let backup = root
        .path()
        .join(
            report
                .backup_relative_path
                .as_deref()
                .expect("迁移备份路径存在"),
        )
        .join("before/managed/secrets/vault.toml");
    assert_eq!(
        fs::read(backup).expect("大 vault 原始备份完整"),
        original_bytes
    );

    let migrated = fs::read_to_string(&vault_path).expect("回读大 vault 迁移结果");
    assert!(migrated.len() > limit);
    assert_eq!(migrated.matches(&ciphertext).count(), 3);
    let normalized = ["target", "jump-0", "jump-1"]
        .into_iter()
        .fold(migrated, |source, scope| {
            let reference =
                format!("credential:kerminal:ssh-host:fixture-host:{scope}:password:v1");
            let aad = BASE64.encode(reference.as_bytes());
            source.replace(
                &format!("associated_data = \"{aad}\"\n"),
                "associated_data = \"\"\n",
            )
        });
    assert_eq!(normalized, source, "除 AAD 外所有密文与格式字节保持不变");
    assert!(!root.path().join("secrets/vault-key.toml").exists());
}

/// 校验完全缺失的 AAD 字段也能迁移，且原有密文元数据之外无文件内容变化。
#[test]
fn absent_aad_field_is_added_without_reserializing_other_fields() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let source = fixture_with_empty_aad_and_user_data();
    let source = source.replace("associated_data = \"\" # AAD 注释应保留\n", "");
    install_vault(root.path(), &source);

    FileDataMigrationRepository::new(root.path())
        .migrate()
        .expect("缺失字段的 schema 1 vault 可迁移");
    let migrated =
        fs::read_to_string(root.path().join("secrets/vault.toml")).expect("回读缺失字段迁移结果");
    let aad = BASE64.encode(FIXTURE_REFERENCE.as_bytes());
    let without_new_aad = migrated.replace(&format!("associated_data = \"{aad}\"\n"), "");
    assert_eq!(without_new_aad, source);
}

/// 校验非空错误 AAD 不会被 planner 改写、备份或发布格式标记。
#[test]
fn invalid_nonempty_aad_blocks_migration_without_overwrite() {
    let original = std::str::from_utf8(FIXTURE_VAULT).expect("fixture 为 UTF-8");
    let expected_aad = BASE64.encode(FIXTURE_REFERENCE.as_bytes());
    let wrong_aad = BASE64.encode(b"credential:kerminal:ssh-host:other-host:target:password:v1");
    for invalid_aad in [wrong_aad, "%%%".to_owned()] {
        let root = tempfile::tempdir().expect("隔离迁移根可创建");
        let source = original.replace(&expected_aad, &invalid_aad);
        assert_ne!(source, original);
        install_vault(root.path(), &source);
        let vault_path = root.path().join("secrets/vault.toml");
        let original_bytes = fs::read(&vault_path).expect("保存错误输入");
        let repository = FileDataMigrationRepository::new(root.path());

        let inspection = repository.inspect().expect("坏 AAD 应转成预检阻断");
        assert_eq!(inspection.readiness, DataMigrationReadiness::Blocked);
        assert!(inspection.blockers.iter().any(|blocker| matches!(
            blocker,
            DataMigrationBlocker::ManagedDataInvalid { relative_path }
                if relative_path == "secrets/vault.toml"
        )));
        assert!(repository.migrate().is_err());
        assert_eq!(
            fs::read(vault_path).expect("错误输入仍保留"),
            original_bytes
        );
        assert!(!root.path().join(".kerminal-format.toml").exists());
    }
}

/// 校验未来 vault schema 进入可见阻断状态，不被一次性迁移重序列化或覆盖。
#[test]
fn future_vault_schema_blocks_migration_without_overwrite() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let original = std::str::from_utf8(FIXTURE_VAULT).expect("fixture 为 UTF-8");
    let source = original.replacen("schema_version = 1", "schema_version = 2", 1);
    install_vault(root.path(), &source);
    let vault_path = root.path().join("secrets/vault.toml");
    let original_bytes = fs::read(&vault_path).expect("保存未来 schema 输入");
    let repository = FileDataMigrationRepository::new(root.path());

    let inspection = repository.inspect().expect("未来 schema 应转成预检阻断");
    assert_eq!(inspection.readiness, DataMigrationReadiness::Blocked);
    assert!(repository.migrate().is_err());
    assert_eq!(
        fs::read(vault_path).expect("未来格式文件仍保留"),
        original_bytes
    );
    assert!(!root.path().join(".kerminal-format.toml").exists());
}
