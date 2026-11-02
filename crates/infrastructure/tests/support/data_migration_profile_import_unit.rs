//! 显式导入使用独立来源、当前目标及同一可恢复事务。
//!
//! @author kongweiguang

use std::{fs, path::Path};

use crate::data_migration::FileDataMigrationRepository;
use kerminal_application::data_migration::{DataMigrationOutcome, DataMigrationRepository};

/// 构造带分组和敏感环境的旧来源，用于验证原文不被修改。
fn legacy_source(root: &Path) -> String {
    fs::create_dir_all(root.join("profiles")).unwrap();
    fs::create_dir_all(root.join("hosts")).unwrap();
    fs::write(
        root.join("hosts/groups.toml"),
        concat!(
            "schema_version = 1\n[[groups]]\nid = \"ops\"\nname = \"Ops\"\n",
            "sort_order = 0\ncreated_at = \"1\"\nupdated_at = \"2\"\n",
        ),
    )
    .unwrap();
    let profile = concat!(
        "schema_version = 1\nid = \"work\"\nname = \"Work\"\nshell = \"cmd.exe\"\n",
        "args = []\nenv = { PASSWORD = \"test-import-secret\" }\nis_default = true\n",
        "sidebar_group_id = \"ops\"\nsort_order = 0\ncreated_at = \"1\"\nupdated_at = \"2\"\n",
    )
    .to_owned();
    fs::write(root.join("profiles/work.toml"), &profile).unwrap();
    profile
}

/// 预演不落盘，提交保留分组和密文，重复导入复用来源身份而不是重复创建主机。
#[test]
fn import_is_read_only_when_previewed_and_idempotent_after_commit() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    let target = parent.path().join("target");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    let original = legacy_source(&source);
    fs::write(
        target.join(".kerminal-format.toml"),
        b"data_format = 4\nmigration_contract = 3\n",
    )
    .unwrap();
    let repository = FileDataMigrationRepository::new(&target);
    let preview = repository
        .preview_legacy_profiles(&source)
        .expect("可只读预演");
    assert_eq!(preview.source_profiles, 1);
    assert_eq!(preview.new_profiles, 1);
    assert_eq!(preview.new_hosts, 1);
    assert!(preview.groups_changed && preview.vault_changed);
    assert!(!target.join("hosts").exists());
    assert!(!target.join("secrets").exists());
    assert!(!source.join(".storage.lock").exists());

    let report = repository
        .import_legacy_profiles(&source)
        .expect("实际导入成功");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    assert!(report.backup_relative_path.is_some());
    assert_eq!(
        fs::read_to_string(source.join("profiles/work.toml")).unwrap(),
        original
    );
    assert!(!source.join(".storage.lock").exists());
    let groups = fs::read_to_string(target.join("hosts/groups.toml")).unwrap();
    assert!(groups.contains("ops"));
    let template = fs::read_to_string(target.join("profiles/work.toml")).unwrap();
    assert!(!template.contains("sidebar_group_id"));
    let host_id = format!("local-{}", super::super::journal::sha256(b"work"));
    let host = fs::read_to_string(target.join("hosts").join(format!("{host_id}.toml"))).unwrap();
    assert!(!host.contains("test-import-secret"));
    assert!(
        !fs::read_to_string(target.join("secrets/vault.toml"))
            .unwrap()
            .contains("test-import-secret")
    );
    let repeated = repository
        .preview_legacy_profiles(&source)
        .expect("重复预演");
    assert_eq!(repeated.new_profiles, 0);
    assert_eq!(repeated.new_hosts, 0);
    assert!(!repeated.groups_changed && !repeated.vault_changed);
    assert_eq!(
        repository
            .import_legacy_profiles(&source)
            .expect("重复导入")
            .outcome,
        DataMigrationOutcome::AlreadyCurrent
    );
}

/// 同来源主机编辑后不能被后续导入覆盖，也不能换一个 ID 重复创建。
#[test]
fn edited_imported_host_blocks_repeat_without_overwriting() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    let target = parent.path().join("target");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    legacy_source(&source);
    fs::write(
        target.join(".kerminal-format.toml"),
        b"data_format = 4\nmigration_contract = 3\n",
    )
    .unwrap();
    let repository = FileDataMigrationRepository::new(&target);
    repository
        .import_legacy_profiles(&source)
        .expect("首轮导入");
    let host_id = format!("local-{}", super::super::journal::sha256(b"work"));
    let path = target.join("hosts").join(format!("{host_id}.toml"));
    let changed = fs::read_to_string(&path)
        .unwrap()
        .replace("name = \"Work\"", "name = \"Edited\"");
    fs::write(&path, &changed).unwrap();
    assert!(repository.import_legacy_profiles(&source).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), changed);
    assert_eq!(fs::read_dir(target.join("hosts")).unwrap().count(), 2);
}

/// 来源和目标指向相同或嵌套目录时阻止自导入和复制环。
#[test]
fn import_rejects_same_or_nested_roots() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("child");
    fs::create_dir(&nested).unwrap();
    fs::write(
        root.path().join(".kerminal-format.toml"),
        b"data_format = 4\nmigration_contract = 3\n",
    )
    .unwrap();
    let repository = FileDataMigrationRepository::new(root.path());
    assert!(repository.preview_legacy_profiles(root.path()).is_err());
    assert!(repository.preview_legacy_profiles(&nested).is_err());
}
