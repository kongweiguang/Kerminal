//! Profile 拆分、环境密封和稳定身份的隔离迁移回归。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, fs, path::Path};

use kerminal_application::data_migration::{
    DataMigrationOutcome, DataMigrationReadiness, DataMigrationRepository,
};
use serde_json::json;

use super::*;
use crate::data_migration::FileDataMigrationRepository;

/// 创建包含用户环境与分组的旧模板，覆盖最容易丢失的数据字段。
fn write_legacy_profile(root: &Path, id: &str, group: bool) -> String {
    fs::create_dir_all(root.join("profiles")).expect("创建隔离 Profile 目录");
    let group_field = if group {
        "sidebar_group_id = \"ops\"\n"
    } else {
        ""
    };
    let source = format!(
        concat!(
            "schema_version = 1\nid = \"{id}\"\nname = \"Legacy Shell\"\n",
            "shell = \"powershell.exe\"\nargs = [\"-NoLogo\"]\ncwd = \"C:/work\"\n",
            "env = {{ PASSWORD = \"test-secret-value\", PROJECT = \"demo\" }}\n",
            "is_default = true\n{group_field}sort_order = 2\ncreated_at = \"1\"\nupdated_at = \"2\"\n",
        ),
        id = id,
        group_field = group_field
    );
    fs::write(root.join("profiles").join(format!("{id}.toml")), &source)
        .expect("写入隔离旧 Profile");
    source
}

/// 从旧契约完成整套迁移，真实 Vault 解密、原文备份和工作区目标应一致。
#[test]
fn previous_profile_becomes_independent_host_with_encrypted_environment() {
    let root = tempfile::tempdir().expect("隔离迁移根");
    fs::write(
        root.path().join(".kerminal-format.toml"),
        b"data_format = 4\nmigration_contract = 2\n",
    )
    .expect("旧 marker");
    fs::create_dir(root.path().join("hosts")).expect("主机目录");
    fs::write(
        root.path().join("hosts/groups.toml"),
        concat!(
            "schema_version = 1\n[[groups]]\nid = \"ops\"\nname = \"Ops\"\n",
            "sort_order = 0\ncreated_at = \"1\"\nupdated_at = \"2\"\n",
        ),
    )
    .expect("主机分组");
    let original = write_legacy_profile(root.path(), "legacy-shell", true);

    let mut workspace: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/workspace_session_v3.json"
    ))
    .expect("workspace fixture");
    workspace["terminalPanes"][0]["mode"] = json!("local");
    workspace["terminalPanes"][0]["target"] = json!({"kind":"local", "profileId":"legacy-shell"});
    workspace["terminalPanes"][0]["profileId"] = json!("legacy-shell");
    workspace["terminalPanes"][0]
        .as_object_mut()
        .unwrap()
        .remove("remoteHostId");
    fs::create_dir(root.path().join("workspace")).expect("工作区目录");
    fs::write(
        root.path().join("workspace/session.json"),
        serde_json::to_vec(&workspace).unwrap(),
    )
    .expect("写入旧工作区");

    let repository = FileDataMigrationRepository::new(root.path());
    assert_eq!(
        repository.inspect().expect("旧格式预检").readiness,
        DataMigrationReadiness::Legacy
    );
    let report = repository.migrate().expect("迁移并提交新契约");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    let template = fs::read_to_string(root.path().join("profiles/legacy-shell.toml")).unwrap();
    assert!(!template.contains("sidebar_group_id"));
    assert!(template.contains("test-secret-value"));
    let host_id = format!("local-{}", journal::sha256(b"legacy-shell"));
    let host_text =
        fs::read_to_string(root.path().join("hosts").join(format!("{host_id}.toml"))).unwrap();
    assert!(!host_text.contains("test-secret-value"));
    let host: toml::Value = toml::from_str(&host_text).expect("本地主机 TOML");
    assert_eq!(host["protocol"].as_str(), Some("local"));
    assert_eq!(host["group_id"].as_str(), Some("ops"));
    let reference = host["local"]["environment_ref"].as_str().expect("环境引用");
    let expected = BTreeMap::from([
        ("PASSWORD".to_owned(), "test-secret-value".to_owned()),
        ("PROJECT".to_owned(), "demo".to_owned()),
    ]);
    assert!(
        profile_migration::profile_environment_matches(
            root.path(),
            &host_id,
            Some(reference),
            &expected,
            None
        )
        .expect("真实 Vault 解密验证")
    );
    let vault = fs::read_to_string(root.path().join("secrets/vault.toml")).expect("密文库已生成");
    assert!(!vault.contains("test-secret-value"));
    assert!(root.path().join("secrets/vault-key.toml").is_file());
    let migrated: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("workspace/session.json")).unwrap())
            .unwrap();
    assert_eq!(migrated["terminalPanes"][0]["profileId"], host_id);
    assert_eq!(migrated["terminalPanes"][0]["target"]["profileId"], host_id);
    let backup = root
        .path()
        .join(report.backup_relative_path.unwrap())
        .join("before/managed/profiles/legacy-shell.toml");
    assert_eq!(fs::read_to_string(backup).unwrap(), original);
    assert_eq!(
        repository.migrate().expect("重复调用幂等").outcome,
        DataMigrationOutcome::AlreadyCurrent
    );
}

/// 已存在密文却缺少 key 时绝不生成新密钥覆盖旧密文或推进 marker。
#[test]
fn missing_key_for_existing_vault_blocks_profile_migration() {
    let root = tempfile::tempdir().expect("隔离迁移根");
    fs::write(
        root.path().join(".kerminal-format.toml"),
        b"data_format = 4\nmigration_contract = 2\n",
    )
    .unwrap();
    let original = write_legacy_profile(root.path(), "legacy-shell", false);
    fs::create_dir(root.path().join("secrets")).unwrap();
    let vault = include_str!("../../../../tests/fixtures/vault/vault-v1.toml");
    fs::write(root.path().join("secrets/vault.toml"), vault).unwrap();
    let repository = FileDataMigrationRepository::new(root.path());
    assert_eq!(
        repository.inspect().unwrap().readiness,
        DataMigrationReadiness::Blocked
    );
    assert!(repository.migrate().is_err());
    assert_eq!(
        fs::read_to_string(root.path().join("profiles/legacy-shell.toml")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(root.path().join("secrets/vault.toml")).unwrap(),
        vault
    );
    assert!(!root.path().join("secrets/vault-key.toml").exists());
}

/// 已有同名 host 不按内容猜测归属，选择稳定后缀且不得覆盖原文件。
#[test]
fn colliding_host_id_is_suffixed_without_overwrite() {
    let root = tempfile::tempdir().expect("隔离迁移根");
    let original = write_legacy_profile(root.path(), "legacy-shell", false);
    fs::create_dir(root.path().join("hosts")).unwrap();
    let id = format!("local-{}", journal::sha256(b"legacy-shell"));
    let occupied = root.path().join("hosts").join(format!("{id}.toml"));
    fs::write(&occupied, "# other host identity\n").unwrap();
    let mut vault_edits = Vec::new();
    let plan = plan_profile_hosts(
        root.path(),
        root.path(),
        &mut vault_edits,
        &Default::default(),
    )
    .expect("占用不阻止选择新 ID");
    assert_eq!(plan.mapping.get("legacy-shell"), Some(&format!("{id}-1")));
    assert_eq!(
        fs::read_to_string(occupied).unwrap(),
        "# other host identity\n"
    );
    let host_path = PathBuf::from("hosts").join(format!("{id}-1.toml"));
    let host = plan
        .edits
        .iter()
        .find(|edit| edit.relative_path == host_path);
    assert!(host.is_some_and(|edit| edit.before.is_none()));
    let profile = plan
        .edits
        .iter()
        .find(|edit| edit.relative_path == Path::new("profiles/legacy-shell.toml"))
        .expect("未修改的旧模板仍作为事务哨兵");
    assert_eq!(profile.before.as_deref(), Some(original.as_bytes()));
    assert_eq!(profile.after, original.as_bytes());
    assert_eq!(plan.edits.len(), 2);
}
