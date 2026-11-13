//! 插件布局迁移阶段中断、证据不足与幂等恢复的故障注入测试。
//!
//! 测试同一份 `plugins-layout-v1` 夹具在每个持久阶段冻结后重跑，避免只覆盖成功尾部。
//!
//! @author kongweiguang

use std::path::{Path, PathBuf};

use kerminal_application::data_migration::{
    DataMigrationBlocker, DataMigrationReadiness, DataMigrationRepository,
};
use kerminal_domain::plugins::{
    PendingPluginChange, PluginId, PluginInstallStatus, PluginStoreState, PluginUnsupportedReason,
};

use super::*;

/// 测试索引统一走生产身份解析，避免直接用字符串绕过目录身份规则。
fn id(value: &str) -> PluginId {
    PluginId::parse(value).expect("测试插件身份")
}

/// 夹具目录位于仓库根，复制到临时根后再写入，绝不接触默认用户数据目录。
fn fixture_root(name: &str) -> (tempfile::TempDir, PathBuf) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/plugins-layout-v1")
        .join(name);
    let temp = tempfile::tempdir().expect("隔离数据根");
    let root = temp.path().join("root");
    copy_tree(&source, &root);
    (temp, root)
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("创建夹具目标");
    for entry in fs::read_dir(source).expect("读取夹具目录") {
        let entry = entry.expect("夹具目录项");
        let destination = target.join(entry.file_name());
        let metadata = entry.metadata().expect("夹具元数据");
        if metadata.is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), &destination).expect("复制夹具文件");
        }
    }
}

/// 冻结到指定阶段时文件系统必须与 journal 声明一致；测试直达私有步骤而不改生产路径。
fn stage(root: &Path, target: PluginMigrationPhase) -> (PathBuf, PluginMigrationJournal) {
    let plugins = root.join(PLUGINS_DIRECTORY);
    let inspection = inspect(root).expect("迁移前分类");
    let inventory = inspection.legacy.expect("夹具应进入 schema 1 迁移");
    let mut journal = plan_migration(&plugins, inventory).expect("规划迁移");
    let backup = layout::resolve_relative(root, &backup_relative_in_plugins(&journal).unwrap())
        .expect("备份路径");
    while journal.phase != target {
        match journal.phase {
            PluginMigrationPhase::Prepared => {
                layout::ensure_directory(&plugins, &[MIGRATION_DIRECTORY, &journal.operation_id])
                    .expect("创建操作目录");
                journal.phase = PluginMigrationPhase::OldMoved;
                write_journal(&plugins, &journal, false).expect("写入 OldMoved");
            }
            PluginMigrationPhase::OldMoved => {
                move_old_tree(&plugins, &backup, &journal).expect("备份旧树");
                journal.phase = PluginMigrationPhase::NewInstalled;
                write_journal(&plugins, &journal, false).expect("写入 NewInstalled");
            }
            PluginMigrationPhase::NewInstalled => {
                move_active_packages(&plugins, &backup, &journal).expect("移入活动目录");
                journal.phase = PluginMigrationPhase::StateCommitted;
                write_journal(&plugins, &journal, false).expect("写入 StateCommitted");
            }
            PluginMigrationPhase::StateCommitted => {
                commit_state(&plugins, &journal).expect("提交 schema 2 状态");
                journal.phase = PluginMigrationPhase::Committed;
                write_journal(&plugins, &journal, false).expect("写入 Committed");
            }
            PluginMigrationPhase::Committed => {
                verify_committed(&plugins, &journal).expect("复核提交结果");
                journal.phase = PluginMigrationPhase::Cleaned;
                write_journal(&plugins, &journal, false).expect("写入 Cleaned");
            }
            // 目标阶段最多到 Cleaned；到达前必须先写入该阶段，不能提前清理。
            PluginMigrationPhase::Cleaned => break,
        }
    }
    (plugins, journal)
}

fn backup_path(root: &Path, journal: &PluginMigrationJournal) -> PathBuf {
    layout::resolve_relative(root, &backup_relative_in_plugins(journal).unwrap()).expect("备份路径")
}

fn read_state(plugins: &Path) -> PluginStoreState {
    serde_json::from_slice(&fs::read(plugins.join(STATE_FILE)).expect("读取迁移后状态"))
        .expect("schema 2 状态")
}

/// 任一阶段冻结后都必须能继续到清理完成，且再次 apply 只报告 AlreadyCurrent。
#[test]
fn every_persisted_phase_resumes_and_cleans_idempotently() {
    for target in [
        PluginMigrationPhase::Prepared,
        PluginMigrationPhase::OldMoved,
        PluginMigrationPhase::NewInstalled,
        PluginMigrationPhase::StateCommitted,
        PluginMigrationPhase::Committed,
        PluginMigrationPhase::Cleaned,
    ] {
        let (_temp, root) = fixture_root("normal");
        let (plugins, _journal) = stage(&root, target);
        let report = migrate_locked(&root).expect("阶段恢复必须成功");
        assert_eq!(report.outcome, PluginLayoutOutcome::Recovered);
        assert_eq!(report.migrated_plugins, 1, "只有 acme.notes 有活动目录");
        assert_eq!(report.removed_previous_versions, 1);
        assert!(!plugins.join(MIGRATION_DIRECTORY).exists());

        let state = read_state(&plugins);
        assert_eq!(state.schema, CURRENT_STATE_SCHEMA);
        assert!(
            state
                .plugins
                .get(&id("acme.notes"))
                .expect("acme.notes")
                .previous
                .is_none()
        );
        assert!(plugins.join("acme.notes/kerminal-plugin.json").is_file());
        assert!(!plugins.join(LEGACY_PACKAGES_DIRECTORY).exists());
        assert!(plugins.join("data/acme.notes/note.txt").is_file());
        assert!(plugins.join("logs/acme.notes.log").is_file());

        let again = migrate_locked(&root).expect("幂等重跑");
        assert_eq!(again.outcome, PluginLayoutOutcome::AlreadyCurrent);
        assert_eq!(again.migrated_plugins, 0);
        assert!(again.backup_relative_path.is_none());
    }
}

/// Cleaned 只表示清理被打断；重跑必须真正删除备份和操作目录，而不是报告恢复完成。
#[test]
fn cleaned_phase_retries_cleanup_until_backup_and_journal_are_gone() {
    let (_temp, root) = fixture_root("normal");
    let (plugins, journal) = stage(&root, PluginMigrationPhase::Cleaned);
    let backup = backup_path(&root, &journal);
    assert!(backup.is_dir(), "冻结阶段应保留待清理备份");
    assert!(
        plugins
            .join(MIGRATION_DIRECTORY)
            .join(&journal.operation_id)
            .join(JOURNAL_FILE)
            .is_file()
    );

    migrate_locked(&root).expect("清理重试成功");
    assert!(!backup.exists());
    assert!(
        !plugins
            .join(MIGRATION_DIRECTORY)
            .join(&journal.operation_id)
            .exists()
    );
}

/// 活动目录与备份同时缺失时不能猜成已移动；现场 journal 与旧状态必须保留。
#[test]
fn insufficient_move_evidence_blocks_without_destroying_backup() {
    let (_temp, root) = fixture_root("normal");
    let (plugins, journal) = stage(&root, PluginMigrationPhase::NewInstalled);
    let backup = backup_path(&root, &journal);
    fs::remove_dir_all(backup.join("packages/acme.notes/1.0.0")).expect("制造双方缺失");
    assert!(!plugins.join("acme.notes").exists());

    let error = migrate_locked(&root).expect_err("双方缺失必须阻断");
    assert_eq!(
        error.public_code(),
        AppError::outcome_unknown("插件目录移动结果未知").public_code()
    );
    assert!(
        plugins
            .join(MIGRATION_DIRECTORY)
            .join(&journal.operation_id)
            .join(JOURNAL_FILE)
            .is_file()
    );
    let legacy = fs::read_to_string(backup.join(STATE_FILE)).expect("旧状态仍在备份中");
    assert!(legacy.contains("\"schema\": 1"));
    assert!(!plugins.join(STATE_FILE).exists());
}

/// 旧状态记录存在但目录缺失时按 Missing 迁移，不因空 source 卡死 journal 校验。
#[test]
fn missing_record_migrates_without_fingerprint() {
    let (_temp, root) = fixture_root("normal");
    let (plugins, _journal) = stage(&root, PluginMigrationPhase::Prepared);
    let report = migrate_locked(&root).expect("含 Missing 记录的旧状态必须可迁移");
    assert_eq!(report.outcome, PluginLayoutOutcome::Recovered);
    let state = read_state(&plugins);
    let missing = state
        .plugins
        .get(&id("acme.missing"))
        .expect("acme.missing");
    assert_eq!(missing.status, PluginInstallStatus::Missing);
    assert!(missing.fingerprint.is_none());
    assert!(missing.previous.is_none());
    assert!(!plugins.join("acme.missing").exists());
}

/// 仅有 Missing 记录、没有 packages 目录的最小旧仓库也能完成迁移。
#[test]
fn missing_only_store_migrates_without_package_tree() {
    let (_temp, root) = fixture_root("missing-record");
    let report = migrate_locked(&root).expect("缺失记录仓库迁移");
    let plugins = root.join(PLUGINS_DIRECTORY);
    assert_eq!(report.outcome, PluginLayoutOutcome::Migrated);
    assert_eq!(report.migrated_plugins, 0);
    let state = read_state(&plugins);
    assert_eq!(
        state
            .plugins
            .get(&id("acme.missing"))
            .expect("acme.missing")
            .status,
        PluginInstallStatus::Missing
    );
    assert!(!plugins.join(MIGRATION_DIRECTORY).exists());
}

/// 丢弃安装/更新/回退意图时保留可撤销失败绑定，pending_failures 只保留仍有 pending 的项。
#[test]
fn pending_conversion_keeps_startup_safe_operations_only() {
    let (_temp, root) = fixture_root("pending");
    let report = migrate_locked(&root).expect("pending 迁移");
    assert_eq!(report.discarded_pending, 3);
    assert_eq!(report.removed_previous_versions, 1);
    let plugins = root.join(PLUGINS_DIRECTORY);
    let state = read_state(&plugins);
    assert_eq!(state.pending.len(), 2);
    assert!(matches!(
        state.pending[0],
        PendingPluginChange::Enable { .. }
    ));
    match &state.pending[1] {
        PendingPluginChange::Uninstall { id, operation_id } => {
            assert_eq!(id.as_str(), "acme.removed");
            assert!(Uuid::parse_str(operation_id).is_ok());
        }
        other => panic!("unexpected pending {other:?}"),
    }
    assert_eq!(
        state.failures.get(&id("acme.notes")).map(String::as_str),
        Some("旧启用失败")
    );
    assert_eq!(
        state
            .failures
            .get(&id("acme.discarded"))
            .map(String::as_str),
        Some(DISCARDED_PENDING_FAILURE)
    );
    assert_eq!(
        state.failures.get(&id("acme.update")).map(String::as_str),
        Some(DISCARDED_PENDING_FAILURE)
    );
    assert!(state.pending_failures.contains(&id("acme.notes")));
    assert!(!state.pending_failures.contains(&id("acme.discarded")));
    assert!(!state.pending_failures.contains(&id("acme.update")));
}

/// 旧外部记录占用内置身份时，其启停 pending 不能被 schema 2 重解释成内置开关。
#[test]
fn reserved_identity_pending_is_discarded_without_touching_builtins() {
    let fixture = tempfile::tempdir().expect("隔离数据根");
    let plugins = fixture.path().join("plugins");
    fs::create_dir_all(&plugins).expect("插件目录");
    fs::write(
        plugins.join(STATE_FILE),
        r#"{"schema":1,"revision":5,"plugins":{"kerminal.ssh":{"active":"1.0.0","enabled":true,"source":"local","installed_at":"2026-07-08T09:10:11Z"}},"pending":[{"op":"enable","id":"kerminal.ssh"}],"failures":{},"pending_failures":[]}"#,
    )
    .expect("旧状态");

    let report = migrate_locked(fixture.path()).expect("保留身份记录迁移");
    assert_eq!(report.discarded_pending, 1);
    let state = read_state(&plugins);
    assert!(state.pending.is_empty());
    assert!(state.builtins.is_empty());
    let record = state
        .plugins
        .get(&id("kerminal.ssh"))
        .expect("外部记录保留");
    assert_eq!(record.status, PluginInstallStatus::Missing);
    assert!(record.fingerprint.is_none());
    assert!(!plugins.join("kerminal.ssh").exists());
}

/// 旧 script 包照常搬目录但标记 Unsupported，内容原字节保留供用户卸载。
#[test]
fn script_package_moves_unchanged_and_is_marked_unsupported() {
    let (_temp, root) = fixture_root("script");
    let plugins = root.join(PLUGINS_DIRECTORY);
    let before = fs::read(plugins.join("packages/acme.scripted/1.0.0/kerminal-plugin.json"))
        .expect("旧 script 清单");
    let report = migrate_locked(&root).expect("script 包迁移");
    assert_eq!(report.migrated_plugins, 1);
    let state = read_state(&plugins);
    assert_eq!(
        state
            .plugins
            .get(&id("acme.scripted"))
            .expect("acme.scripted")
            .status,
        PluginInstallStatus::Unsupported {
            reason: PluginUnsupportedReason::ScriptRemoved
        }
    );
    assert_eq!(
        fs::read(plugins.join("acme.scripted/kerminal-plugin.json")).expect("迁移后清单"),
        before
    );
}

/// schema 1 空仓库单独转换到 schema 2，不留下迁移控制目录。
#[test]
fn empty_schema_one_store_becomes_current() {
    let (_temp, root) = fixture_root("empty");
    let plugins = root.join(PLUGINS_DIRECTORY);
    let report = migrate_locked(&root).expect("空仓库迁移");
    assert_eq!(report.outcome, PluginLayoutOutcome::Migrated);
    let state = read_state(&plugins);
    assert_eq!(state.schema, CURRENT_STATE_SCHEMA);
    assert!(state.plugins.is_empty());
    assert!(!plugins.join(MIGRATION_DIRECTORY).exists());
    assert_eq!(
        inspect(&root).expect("迁移后分类").class,
        PluginLayoutClass::Current
    );
}

/// 损坏、未来 schema、残留 staging 与未知材料只能只读阻断，原文不能被覆盖。
#[test]
fn unknown_or_residual_material_blocks_without_modifying_originals() {
    for (fixture, expected) in [
        (
            "corrupt-state",
            DataMigrationBlocker::PluginLayoutStateInvalid { schema: None },
        ),
        (
            "future-state",
            DataMigrationBlocker::PluginLayoutStateInvalid { schema: Some(3) },
        ),
        (
            "residual-staging",
            DataMigrationBlocker::PluginLayoutResidualMaterial,
        ),
        (
            "unknown-material",
            DataMigrationBlocker::PluginLayoutResidualMaterial,
        ),
    ] {
        let (_temp, root) = fixture_root(fixture);
        let plugins = root.join(PLUGINS_DIRECTORY);
        let before = snapshot(&plugins);
        let inspection = inspect(&root).expect("只读分类");
        assert!(
            inspection.blockers.contains(&expected),
            "{fixture}: {:?}",
            inspection.blockers
        );
        assert_eq!(inspection.readiness(), PluginLayoutReadiness::Blocked);
        assert!(migrate_locked(&root).is_err());
        assert_eq!(snapshot(&plugins), before, "{fixture} 原文必须保留");
        assert!(!plugins.join(MIGRATION_DIRECTORY).exists());
    }
}

/// 正常安装窗口：pending 引用的 `.staging/<id>/<uuid>` 是已知材料，不能阻断启动快照。
#[test]
fn pending_referenced_staging_slot_is_current_and_unowned_slot_blocks() {
    let fixture = tempfile::tempdir().expect("隔离数据根");
    let root = fixture.path().join("root");
    let plugins = root.join(PLUGINS_DIRECTORY);
    let slot = plugins
        .join(STAGING_DIRECTORY)
        .join("acme.staged")
        .join("00000000-0000-4000-8000-000000000011");
    fs::create_dir_all(&slot).expect("创建暂存槽");
    fs::write(slot.join(MANIFEST_FILE), b"{}").expect("暂存清单");
    // 正常安装会留下空的解包预热目录；没有内容时不算残留。
    fs::create_dir_all(plugins.join(STAGING_DIRECTORY).join(INCOMING_DIRECTORY)).expect("预热目录");

    write_pending_install_state(&plugins, true);
    let inspection = inspect(&root).expect("只读分类");
    assert!(inspection.blockers.is_empty(), "{:?}", inspection.blockers);
    assert_eq!(inspection.class, PluginLayoutClass::Current);
    assert!(!inspection.requires_migration());
    assert_eq!(inspection.readiness(), PluginLayoutReadiness::Current);
    let root_inspection =
        super::super::inspection::inspect_root(&root, None, None).expect("数据迁移预检");
    assert!(
        !root_inspection
            .blockers
            .contains(&DataMigrationBlocker::UnknownNonEmptyRoot)
    );
    assert_ne!(root_inspection.readiness, DataMigrationReadiness::Blocked);

    // 同一槽位失去 pending 归属后仍必须阻断并保留现场。
    write_pending_install_state(&plugins, false);
    let inspection = inspect(&root).expect("无归属分类");
    assert_eq!(inspection.readiness(), PluginLayoutReadiness::Blocked);
    assert!(
        inspection
            .blockers
            .contains(&DataMigrationBlocker::PluginLayoutResidualMaterial)
    );
    assert!(slot.is_dir());
}

/// 只写 schema 2 状态与暂存槽，避免走安装器并验证分类不依赖安装内容校验。
fn write_pending_install_state(plugins: &Path, pending: bool) {
    let pending = if pending {
        r#"[{"op":"install","id":"acme.staged","to":"1.0.0","staged":"00000000-0000-4000-8000-000000000011","fingerprint":"v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","source":"official","installed_at":"2026-05-06T07:08:09Z"}]"#
    } else {
        "[]"
    };
    let state = format!(
        r#"{{"schema":2,"revision":1,"plugins":{{}},"pending":{pending},"failures":{{}}}}"#
    );
    fs::write(plugins.join(STATE_FILE), state).expect("写入 schema 2 状态");
}

/// 插件迁移先完成、数据 marker 最后写出；仅剩 plugins 根时仍须被识别为受管根并收尾。
#[test]
fn committed_journal_without_marker_is_managed_and_finishes() {
    let (_temp, root) = fixture_root("normal");
    // Committed 表示 schema 2 状态已提交、清理尚未执行；这正是 marker 缺失的中断窗口。
    let (plugins, _journal) = stage(&root, PluginMigrationPhase::Committed);
    assert!(!root.join(".kerminal-format.toml").exists());
    assert!(plugins.join(STATE_FILE).is_file());

    let inspection =
        super::super::inspection::inspect_root(&root, None, None).expect("中断窗口预检");
    assert!(
        !inspection
            .blockers
            .contains(&DataMigrationBlocker::UnknownNonEmptyRoot),
        "{:?}",
        inspection.blockers
    );
    assert_ne!(inspection.readiness, DataMigrationReadiness::Blocked);
    assert!(inspection.recovery_pending);

    let repository = crate::data_migration::FileDataMigrationRepository::new(&root);
    repository.migrate().expect("迁移收尾并写出 marker");
    assert!(root.join(".kerminal-format.toml").is_file());
    assert!(!plugins.join(MIGRATION_DIRECTORY).exists());
}

/// 同一插件出现多条 pending 时无法明确保留顺序，必须交可见恢复而不是猜一条执行。
#[test]
fn duplicate_pending_for_one_plugin_blocks_automatic_migration() {
    let fixture = tempfile::tempdir().expect("隔离数据根");
    let plugins = fixture.path().join("plugins");
    fs::create_dir_all(&plugins).expect("插件目录");
    let state = r#"{"schema":1,"revision":0,"plugins":{},"pending":[{"op":"enable","id":"acme.notes"},{"op":"disable","id":"acme.notes"}],"failures":{},"pending_failures":[]}"#;
    fs::write(plugins.join(STATE_FILE), state).expect("旧状态");
    let inspection = inspect(fixture.path()).expect("只读分类");
    assert_eq!(inspection.readiness(), PluginLayoutReadiness::Blocked);
    assert!(
        inspection
            .blockers
            .contains(&DataMigrationBlocker::PluginLayoutStateInvalid {
                schema: Some(LEGACY_STATE_SCHEMA)
            })
    );
    assert!(migrate_locked(fixture.path()).is_err());
    assert_eq!(
        fs::read_to_string(plugins.join(STATE_FILE)).expect("原文保留"),
        state
    );
}

/// 只比较稳定的相对路径和字节，防止测试自身修改顺序掩盖原文变化。
fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, current: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(current).expect("读取快照目录") {
            let entry = entry.expect("快照项");
            let path = entry.path();
            let metadata = entry.metadata().expect("快照元数据");
            if metadata.is_dir() {
                walk(root, &path, entries);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("相对路径")
                    .to_string_lossy()
                    .replace('\\', "/");
                entries.push((relative, fs::read(&path).expect("快照内容")));
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}
