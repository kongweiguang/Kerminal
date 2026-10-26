//! 已完成的旧版文件事务不能阻断迁移，未完成或无法核对的材料仍须保留并阻断。
//!
//! @author kongweiguang

use std::{fs, path::Path};

use kerminal_application::data_migration::{
    DataMigrationBlocker, DataMigrationReadiness, DataMigrationRepository,
};
use kerminal_infrastructure::data_migration::FileDataMigrationRepository;

const CHANGE_ID: &str = "profiles-legacy-fixture";

/// 构造与旧 FileStore 相同的终态 journal/manifest，不复制任何真实用户配置或凭据。
fn terminal_archive(root: &Path, rolled_back: bool) -> (String, String) {
    let status = if rolled_back { "repaired" } else { "applied" };
    let phase = if rolled_back {
        "rolled_back"
    } else {
        "committed"
    };
    let file = if rolled_back {
        "rolled-back.toml"
    } else {
        "committed.toml"
    };
    let manifest = format!(
        "schema_version = 1\n[[change_sets]]\nid = \"{CHANGE_ID}\"\nstatus = \"{status}\"\nstarted_at = \"123\"\ncompleted_at = \"124\"\ntouched_files = [\"settings.toml\"]\n"
    );
    let journal = format!(
        "schema_version = 1\nid = \"{CHANGE_ID}\"\nstarted_at = \"123\"\nphase = \"{phase}\"\n[[entries]]\nrelative_path = \"settings.toml\"\noriginal_exists = false\naction = \"write\"\nstaged_path = \".storage-transactions/{CHANGE_ID}/staged/settings.toml\"\nintended_sha256 = \"sha256:{}\"\n",
        "a".repeat(64)
    );
    let directory = root.join(".storage-transactions").join(CHANGE_ID);
    fs::create_dir_all(&directory).expect("创建隔离旧事务目录");
    fs::write(directory.join(file), &journal).expect("写入旧事务终态");
    fs::write(root.join("storage-manifest.toml"), &manifest).expect("写入旧事务总清单");
    fs::write(root.join("settings.toml"), "schema_version = 1\n").expect("写入当前有效配置");
    (manifest, journal)
}

/// 通过公开入口验证 inspect 不写入，apply 保留旧存档且不回放历史 after，随后保持幂等。
#[test]
fn completed_legacy_archives_migrate_without_replaying_or_deleting_them() {
    for rolled_back in [false, true] {
        let root = tempfile::tempdir().expect("创建隔离根");
        let (manifest, journal) = terminal_archive(root.path(), rolled_back);
        let repository = FileDataMigrationRepository::new(root.path());
        let inspection = repository.inspect().expect("只读预检");
        assert_eq!(inspection.readiness, DataMigrationReadiness::Legacy);
        assert!(inspection.blockers.is_empty());
        assert!(!root.path().join(".storage.lock").exists());
        assert!(!root.path().join(".kerminal-migration").exists());
        repository.migrate().expect("终态存档允许迁移");
        assert_eq!(
            fs::read_to_string(root.path().join("storage-manifest.toml")).unwrap(),
            manifest
        );
        let file = if rolled_back {
            "rolled-back.toml"
        } else {
            "committed.toml"
        };
        assert_eq!(
            fs::read_to_string(
                root.path()
                    .join(".storage-transactions")
                    .join(CHANGE_ID)
                    .join(file)
            )
            .unwrap(),
            journal
        );
        assert_eq!(
            fs::read_to_string(root.path().join("settings.toml")).unwrap(),
            "schema_version = 1\n"
        );
        assert_eq!(
            repository.inspect().unwrap().readiness,
            DataMigrationReadiness::Current
        );
        assert_eq!(repository.migrate().unwrap().changed_file_count, 0);
    }
}

/// 即使存在终态收据，同目录的 pending 或未知材料也不能被误当作可忽略的历史存档。
#[test]
fn pending_or_unknown_material_beside_terminal_receipt_still_blocks() {
    for name in ["pending.toml", "unknown.txt", "staged"] {
        let root = tempfile::tempdir().expect("创建隔离根");
        terminal_archive(root.path(), false);
        let extra = root
            .path()
            .join(".storage-transactions")
            .join(CHANGE_ID)
            .join(name);
        if name == "staged" {
            fs::create_dir(&extra).unwrap();
        } else {
            fs::write(&extra, "preserve recovery evidence").unwrap();
        }
        let repository = FileDataMigrationRepository::new(root.path());
        let inspection = repository.inspect().unwrap();
        assert_eq!(inspection.readiness, DataMigrationReadiness::Blocked);
        assert!(inspection.blockers.iter().any(|blocker| matches!(
            blocker,
            DataMigrationBlocker::PendingLegacyTransaction { .. }
        )));
        assert!(repository.migrate().is_err());
        assert!(extra.exists());
        assert!(!root.path().join(".kerminal-format.toml").exists());
    }
}

/// 终态文件必须与总清单、目录身份及已知 schema 一致，不能仅凭 committed 文件名放行。
#[test]
fn unverified_terminal_receipts_remain_blocked() {
    for defect in [
        "phase",
        "id",
        "schema",
        "manifest",
        "paths",
        "missing_manifest",
        "corrupt",
    ] {
        let root = tempfile::tempdir().expect("创建隔离根");
        let (mut manifest, mut journal) = terminal_archive(root.path(), false);
        match defect {
            "phase" => journal = journal.replace("phase = \"committed\"", "phase = \"applying\""),
            "id" => {
                journal = journal.replace(&format!("id = \"{CHANGE_ID}\""), "id = \"another-id\"")
            }
            "schema" => journal = journal.replace("schema_version = 1", "schema_version = 99"),
            "manifest" => {
                manifest = manifest.replace("status = \"applied\"", "status = \"repaired\"")
            }
            "paths" => {
                manifest = manifest.replace(
                    "touched_files = [\"settings.toml\"]",
                    "touched_files = [\"hosts/another.toml\"]",
                )
            }
            "corrupt" => journal = "invalid [toml".to_owned(),
            "missing_manifest" => {}
            _ => unreachable!(),
        }
        fs::write(root.path().join("storage-manifest.toml"), &manifest).unwrap();
        if defect == "missing_manifest" {
            fs::remove_file(root.path().join("storage-manifest.toml")).unwrap();
        }
        let receipt = root
            .path()
            .join(".storage-transactions")
            .join(CHANGE_ID)
            .join("committed.toml");
        fs::write(&receipt, &journal).unwrap();
        let repository = FileDataMigrationRepository::new(root.path());
        assert_eq!(
            repository.inspect().unwrap().readiness,
            DataMigrationReadiness::Blocked,
            "{defect}"
        );
        assert!(repository.migrate().is_err(), "{defect}");
        assert_eq!(fs::read_to_string(receipt).unwrap(), journal);
        assert!(!root.path().join(".kerminal-format.toml").exists());
    }
}
