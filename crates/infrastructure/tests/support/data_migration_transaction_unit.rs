//! 迁移文件事务故障注入、恢复与 Fresh 初始化测试。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::storage::StorageCoordinator;
use kerminal_application::{AppError, data_migration::DataMigrationOutcome};
use uuid::Uuid;

use super::super::{DataMigrationRepository, FileDataMigrationRepository};
use super::*;

const PREVIOUS_GUIDE_V6: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v6.md");

/// 创建一个可真实恢复的 settings 变更，并在提交前发布同一 journal。
fn prepared_transaction(root: &Path) -> MigrationJournal {
    fs::create_dir_all(root).expect("迁移根可创建");
    let before = b"theme = \"system\"\n".to_vec();
    fs::write(root.join("settings.toml"), &before).expect("源设置可写入");
    let operation_id = Uuid::new_v4().to_string();
    let operation_dir = journal::operation_directory(root, &operation_id);
    journal::ensure_directory(root, &operation_dir).expect("操作目录可创建");
    let journal_value = prepare_file_journal(
        root,
        &operation_id,
        vec![
            FileEdit {
                relative_path: "settings.toml".into(),
                before: Some(before),
                after: b"theme = \"dark\"\n".to_vec(),
            },
            FileEdit {
                relative_path: marker::MARKER_FILE_NAME.into(),
                before: None,
                after: marker::current_marker_bytes(),
            },
        ],
        None,
    )
    .expect("文件 staging 和 before 备份可准备");
    journal::write_journal(root, &journal_value, true).expect("prepared journal 可原子发布");
    journal_value
}

/// 在独立的相对路径根完成清理，验证 `./` 与中文目录名不被 private 写入器误拒绝。
struct RelativeRoot(PathBuf);

impl Drop for RelativeRoot {
    /// 测试退出时只移除本测试 UUID 目录，不碰工作区其它数据。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// pending sibling 已发布但目标仍为 before 时，重入应复用 pending 并完成 marker 最后提交。
#[test]
fn interrupted_after_pending_resumes_without_rebuilding_stage() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let mut journal_value = prepared_transaction(root.path());
    let result = resume_journal_with_hook(root.path(), &mut journal_value, |point| {
        if point == FaultPoint::AfterManagedPending(0) {
            Err(AppError::conflict("测试注入的中断"))
        } else {
            Ok(())
        }
    });

    assert!(matches!(result, Err(AppError::Conflict { .. })));
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("before 保留"),
        b"theme = \"system\"\n"
    );
    assert!(matches!(
        marker::read_marker(root.path()).expect("marker 可读"),
        marker::MarkerRead::Missing
    ));
    let recovered = journal::read_active_journal(root.path())
        .expect("active journal 可读")
        .expect("恢复 journal 保留");
    let stage = staged_path(
        &journal::operation_directory(root.path(), &recovered.operation_id),
        "settings.toml",
    )
    .expect("stage 路径固定");
    assert!(!stage.exists());

    resume_journal(root.path(), recovered).expect("pending 可恢复提交");
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("after 已提交"),
        b"theme = \"dark\"\n"
    );
    assert!(matches!(
        marker::read_marker(root.path()).expect("marker 可读"),
        marker::MarkerRead::Current
    ));
    assert_eq!(
        journal::read_active_journal(root.path())
            .expect("active journal 可读")
            .expect("已提交 journal 保留")
            .state,
        JournalState::Committed
    );
}

/// 普通文件已提交但 marker 尚未提交时，重入应识别 after 并只补齐 marker 与 journal。
#[test]
fn interrupted_after_managed_commit_resumes_marker_last() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let mut journal_value = prepared_transaction(root.path());
    let result = resume_journal_with_hook(root.path(), &mut journal_value, |point| {
        if point == FaultPoint::AfterManagedFile(0) {
            Err(AppError::conflict("测试注入的中断"))
        } else {
            Ok(())
        }
    });

    assert!(matches!(result, Err(AppError::Conflict { .. })));
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("业务文件已提交"),
        b"theme = \"dark\"\n"
    );
    assert!(matches!(
        marker::read_marker(root.path()).expect("marker 可读"),
        marker::MarkerRead::Missing
    ));
    let recovered = journal::read_active_journal(root.path())
        .expect("active journal 可读")
        .expect("恢复 journal 保留");
    resume_journal(root.path(), recovered).expect("marker 可恢复提交");

    assert!(matches!(
        marker::read_marker(root.path()).expect("marker 可读"),
        marker::MarkerRead::Current
    ));
    assert_eq!(
        journal::read_active_journal(root.path())
            .expect("active journal 可读")
            .expect("已提交 journal 保留")
            .state,
        JournalState::Committed
    );
}

/// 外部第三值不属于 CAS 的 before/after，恢复必须阻断并保留 marker 未提交与备份。
#[test]
fn third_value_blocks_resume_and_preserves_evidence() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let mut journal_value = prepared_transaction(root.path());
    let _ = resume_journal_with_hook(root.path(), &mut journal_value, |point| {
        if point == FaultPoint::AfterManagedPending(0) {
            Err(AppError::conflict("测试注入的中断"))
        } else {
            Ok(())
        }
    });
    fs::write(root.path().join("settings.toml"), b"theme = \"external\"\n")
        .expect("模拟并发外部写入");
    let recovered = journal::read_active_journal(root.path())
        .expect("active journal 可读")
        .expect("恢复 journal 保留");
    let operation_id = recovered.operation_id.clone();

    assert!(matches!(
        resume_journal(root.path(), recovered),
        Err(AppError::Conflict { .. })
    ));
    assert!(matches!(
        marker::read_marker(root.path()).expect("marker 可读"),
        marker::MarkerRead::Missing
    ));
    let backup = backup_path(
        &journal::operation_directory(root.path(), &operation_id),
        "settings.toml",
    )
    .expect("before 路径固定");
    assert_eq!(
        fs::read(backup).expect("before 备份保留"),
        b"theme = \"system\"\n"
    );
}

/// marker 已提交而 journal 状态写回前中断时，重入完整回读后收敛为 Committed。
#[test]
fn interrupted_after_marker_commit_is_confirmed_on_resume() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let mut journal_value = prepared_transaction(root.path());
    let result = resume_journal_with_hook(root.path(), &mut journal_value, |point| {
        if point == FaultPoint::AfterMarkerCommit {
            Err(AppError::conflict("测试注入的中断"))
        } else {
            Ok(())
        }
    });

    assert!(matches!(result, Err(AppError::Conflict { .. })));
    assert!(matches!(
        marker::read_marker(root.path()).expect("marker 可读"),
        marker::MarkerRead::Current
    ));
    let recovered = journal::read_active_journal(root.path())
        .expect("active journal 可读")
        .expect("恢复 journal 保留");
    assert_eq!(recovered.state, JournalState::Committing);
    resume_journal(root.path(), recovered).expect("已提交 marker 可确认收敛");
    assert_eq!(
        journal::read_active_journal(root.path())
            .expect("active journal 可读")
            .expect("已提交 journal 保留")
            .state,
        JournalState::Committed
    );
}

/// journal 发布结果未知时保留其 staging/backup，确定未发布时才清理本次 UUID 目录。
#[test]
fn uncertain_journal_publish_preserves_recovery_material() {
    let uncertain_root = tempfile::tempdir().expect("隔离迁移根可创建");
    let journal_value = prepared_transaction(uncertain_root.path());
    let operation_dir =
        journal::operation_directory(uncertain_root.path(), &journal_value.operation_id);
    let stage = staged_path(&operation_dir, "settings.toml").expect("stage 路径固定");
    let backup = backup_path(&operation_dir, "settings.toml").expect("backup 路径固定");

    let error = handle_journal_publish_error(
        uncertain_root.path(),
        &journal_value.operation_id,
        AppError::outcome_unknown("测试注入的 journal 回读未知"),
    );
    assert!(matches!(error, AppError::OutcomeUnknown { .. }));
    assert!(
        journal::read_active_journal(uncertain_root.path())
            .expect("已发布 journal 仍可读")
            .is_some()
    );
    assert_eq!(
        fs::read(stage).expect("未知结果 stage 保留"),
        b"theme = \"dark\"\n"
    );
    assert_eq!(
        fs::read(backup).expect("未知结果 backup 保留"),
        b"theme = \"system\"\n"
    );

    let unpublished_root = tempfile::tempdir().expect("另一隔离迁移根可创建");
    let before = b"theme = \"system\"\n".to_vec();
    fs::write(unpublished_root.path().join("settings.toml"), &before).expect("源设置可写入");
    let operation_id = Uuid::new_v4().to_string();
    let operation_dir = journal::operation_directory(unpublished_root.path(), &operation_id);
    journal::ensure_directory(unpublished_root.path(), &operation_dir)
        .expect("未发布操作目录可创建");
    let _ = prepare_file_journal(
        unpublished_root.path(),
        &operation_id,
        vec![
            FileEdit {
                relative_path: "settings.toml".into(),
                before: Some(before),
                after: b"theme = \"dark\"\n".to_vec(),
            },
            FileEdit {
                relative_path: marker::MARKER_FILE_NAME.into(),
                before: None,
                after: marker::current_marker_bytes(),
            },
        ],
        None,
    )
    .expect("未发布 staging 可准备");
    let error = handle_journal_publish_error(
        unpublished_root.path(),
        &operation_id,
        AppError::infrastructure("测试注入的确定未提交"),
    );
    assert!(matches!(error, AppError::Infrastructure { .. }));
    assert!(!operation_dir.exists());
}

/// 新的相对中文数据根应完成完整 Fresh 初始化，第二次调用只返回幂等回执。
#[test]
fn fresh_root_migrates_and_then_is_idempotent() {
    let relative = RelativeRoot(PathBuf::from(format!(
        "./.kerminal-migration-test-{}",
        Uuid::new_v4().simple()
    )));
    fs::create_dir_all(&relative.0).expect("相对中文根可创建");
    let repository = FileDataMigrationRepository::new(relative.0.as_path());

    let first = <FileDataMigrationRepository as DataMigrationRepository>::migrate(&repository)
        .expect("Fresh 根可初始化");
    assert_eq!(first.outcome, DataMigrationOutcome::Initialized);
    assert_eq!(first.changed_file_count, 1);
    assert!(
        first
            .backup_relative_path
            .as_deref()
            .is_some_and(|path| { path.starts_with(journal::MIGRATION_DIRECTORY) })
    );
    assert!(matches!(
        marker::read_marker(&relative.0).expect("marker 可读"),
        marker::MarkerRead::Current
    ));

    let second = <FileDataMigrationRepository as DataMigrationRepository>::migrate(&repository)
        .expect("当前格式重入幂等");
    assert_eq!(second.outcome, DataMigrationOutcome::AlreadyCurrent);
    assert_eq!(second.changed_file_count, 0);
}

/// 未完成的事务目录丢失时只读扫描必须阻断，不能在恢复入口新建空目录伪装可恢复。
#[test]
fn missing_operation_directory_is_not_recreated_by_resume() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let journal_value = prepared_transaction(root.path());
    let operation_id = journal_value.operation_id.clone();
    let operation_dir = journal::operation_directory(root.path(), &operation_id);
    fs::remove_dir_all(&operation_dir).expect("模拟恢复材料丢失");

    assert!(
        !journal::inspect_journals(root.path())
            .expect("journal scanner 返回阻断状态")
            .blockers
            .is_empty()
    );
    assert!(matches!(
        resume_journal(root.path(), journal_value),
        Err(AppError::Conflict { .. })
    ));
    assert!(!operation_dir.exists());
}

/// 当前 marker 下即使另一实例持有根锁，也应在无锁快速路径返回幂等结果。
#[test]
fn current_marker_fast_path_does_not_require_a_storage_lock() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    fs::write(
        root.path().join(marker::MARKER_FILE_NAME),
        marker::current_marker_bytes(),
    )
    .expect("当前 marker 可写入");
    let storage = StorageCoordinator::new(root.path());
    let _held_lock = storage.lock().expect("已持有同一用户数据根锁");
    let repository = FileDataMigrationRepository::new(root.path());
    let report = <FileDataMigrationRepository as DataMigrationRepository>::migrate(&repository)
        .expect("current fastpath 不等待或重入存储锁");
    assert_eq!(report.outcome, DataMigrationOutcome::AlreadyCurrent);
    assert_eq!(report.changed_file_count, 0);
}

/// Current roots update only a frozen guide and keep their format marker and unrelated settings intact.
///
/// @author kongweiguang
#[test]
fn current_contract_root_updates_frozen_guide_and_then_is_idempotent() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let marker_bytes = marker::current_marker_bytes();
    let settings = b"schema_version = 99\nuser_extension = \"untouched\"\n";
    fs::write(root.path().join(marker::MARKER_FILE_NAME), &marker_bytes)
        .expect("current marker 可写入");
    fs::write(root.path().join("kerminal-config.md"), PREVIOUS_GUIDE_V6).expect("冻结旧指南可写入");
    fs::write(root.path().join("settings.toml"), settings).expect("用户设置可写入");
    let repository = FileDataMigrationRepository::new(root.path());

    let updated = repository.migrate().expect("current 根仅更新冻结指南");
    assert_eq!(updated.outcome, DataMigrationOutcome::Migrated);
    assert_eq!(updated.changed_file_count, 1);
    assert!(!updated.resumed);
    assert_eq!(
        fs::read(root.path().join("kerminal-config.md")).expect("新指南可读"),
        crate::configuration::managed_config_guide()
    );
    assert_eq!(
        fs::read(root.path().join(marker::MARKER_FILE_NAME)).expect("marker 原文保留"),
        marker_bytes
    );
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("用户设置保留"),
        settings
    );
    let backup = root.path().join(
        updated
            .backup_relative_path
            .as_deref()
            .expect("指南备份回执"),
    );
    assert_eq!(
        fs::read(backup.join("before/managed/kerminal-config.md")).expect("冻结指南 before 有备份"),
        PREVIOUS_GUIDE_V6.as_bytes()
    );

    let repeated = repository.migrate().expect("再次调用保持幂等");
    assert_eq!(repeated.outcome, DataMigrationOutcome::AlreadyCurrent);
    assert_eq!(repeated.changed_file_count, 0);
    assert!(!repeated.resumed);
}

/// User-edited or unrecognized guide bytes remain caller-owned even when the root marker is current.
///
/// @author kongweiguang
#[test]
fn current_contract_root_preserves_user_edited_guide() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let marker_bytes = marker::current_marker_bytes();
    let custom = format!("{PREVIOUS_GUIDE_V6}\n## My notes\nkeep this text\n");
    fs::write(root.path().join(marker::MARKER_FILE_NAME), &marker_bytes)
        .expect("current marker 可写入");
    fs::write(root.path().join("kerminal-config.md"), &custom).expect("用户指南可写入");
    let repository = FileDataMigrationRepository::new(root.path());

    let report = repository.migrate().expect("用户指南不阻断 current 根");
    assert_eq!(report.outcome, DataMigrationOutcome::AlreadyCurrent);
    assert_eq!(report.changed_file_count, 0);
    assert_eq!(
        fs::read(root.path().join("kerminal-config.md")).expect("用户指南仍可读"),
        custom.as_bytes()
    );
    assert_eq!(
        fs::read(root.path().join(marker::MARKER_FILE_NAME)).expect("marker 原文保留"),
        marker_bytes
    );
    assert!(!root.path().join(journal::MIGRATION_DIRECTORY).exists());
}

/// Noncanonical current markers are preserved instead of entering a journal the current validator cannot verify.
///
/// @author kongweiguang
#[test]
fn current_guide_update_preserves_noncanonical_marker_with_manual_guidance() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let mut marker_bytes = marker::current_marker_bytes();
    marker_bytes.extend_from_slice(b"# keep user marker formatting\n");
    fs::write(root.path().join(marker::MARKER_FILE_NAME), &marker_bytes)
        .expect("带注释 current marker 可写入");
    fs::write(root.path().join("kerminal-config.md"), PREVIOUS_GUIDE_V6).expect("冻结旧指南可写入");
    let repository = FileDataMigrationRepository::new(root.path());

    let error = match repository.migrate() {
        Ok(_) => panic!("非规范 marker 不应启动指南事务"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("[appearance]"));
    assert_eq!(
        fs::read(root.path().join(marker::MARKER_FILE_NAME)).expect("原 marker 保留"),
        marker_bytes
    );
    assert_eq!(
        fs::read(root.path().join("kerminal-config.md")).expect("旧指南保留"),
        PREVIOUS_GUIDE_V6.as_bytes()
    );
    assert!(!root.path().join(journal::MIGRATION_DIRECTORY).exists());
}

/// A current-marker guide journal interrupted after publishing its pending sibling resumes in place.
///
/// @author kongweiguang
#[test]
fn current_guide_transaction_recovers_after_pending_publish_interruption() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let marker_bytes = marker::current_marker_bytes();
    fs::write(root.path().join(marker::MARKER_FILE_NAME), &marker_bytes)
        .expect("current marker 可写入");
    fs::write(root.path().join("kerminal-config.md"), PREVIOUS_GUIDE_V6).expect("冻结旧指南可写入");
    let guide_edit = super::super::configuration::plan_current_config_guide_edit(root.path())
        .expect("current 指南 planner 可读")
        .expect("冻结指南具有更新计划");
    let operation_id = Uuid::new_v4().to_string();
    let operation_dir = journal::operation_directory(root.path(), &operation_id);
    journal::ensure_directory(root.path(), &operation_dir).expect("操作目录可创建");
    let journal_value = prepare_file_journal(
        root.path(),
        &operation_id,
        vec![
            guide_edit,
            FileEdit {
                relative_path: marker::MARKER_FILE_NAME.into(),
                before: Some(marker_bytes.clone()),
                after: marker_bytes.clone(),
            },
        ],
        None,
    )
    .expect("current same-value marker sentinel 可记录");
    // 同值 marker 不能借道恢复其它业务文件，也不能伪造原本缺失的指南。
    let mut unrelated_file = journal_value.clone();
    unrelated_file.files[0].relative_path = "settings.toml".to_owned();
    assert!(verify_marker_before(root.path(), &operation_dir, &unrelated_file).is_err());
    let mut missing_guide_before = journal_value.clone();
    missing_guide_before.files[0].before_sha256 = None;
    assert!(verify_marker_before(root.path(), &operation_dir, &missing_guide_before).is_err());
    journal::write_journal(root.path(), &journal_value, true).expect("journal 可发布");
    let mut interrupted_journal = journal_value.clone();
    let interrupted = resume_journal_with_hook(root.path(), &mut interrupted_journal, |point| {
        if point == FaultPoint::AfterManagedPending(0) {
            Err(AppError::conflict("注入指南 pending 中断"))
        } else {
            Ok(())
        }
    });
    assert!(matches!(interrupted, Err(AppError::Conflict { .. })));

    let repository = FileDataMigrationRepository::new(root.path());
    let resumed = repository.migrate().expect("current 根只恢复原指南事务");
    assert_eq!(resumed.outcome, DataMigrationOutcome::Resumed);
    assert_eq!(resumed.changed_file_count, 1);
    assert!(resumed.resumed);
    assert_eq!(
        fs::read(root.path().join("kerminal-config.md")).expect("原指南 after 完成"),
        crate::configuration::managed_config_guide()
    );
    assert_eq!(
        fs::read(root.path().join(marker::MARKER_FILE_NAME)).expect("current marker 保持不变"),
        marker_bytes
    );
}

/// 使用完整有效配置准备 contract 1 原计划，避免新版 planner 替代中断事务已经发布的 after。
fn previous_contract_transaction(root: &Path) -> MigrationJournal {
    fs::create_dir_all(root).expect("创建隔离迁移根");
    let before = b"schema_version = 1\n# before previous migration\n".to_vec();
    fs::write(root.join("settings.toml"), &before).expect("写入旧配置原文");
    let operation_id = Uuid::new_v4().to_string();
    journal::ensure_directory(root, &journal::operation_directory(root, &operation_id))
        .expect("创建旧操作目录");
    let journal_value = prepare_file_journal(
        root,
        &operation_id,
        vec![
            FileEdit {
                relative_path: "settings.toml".into(),
                before: Some(before),
                after: b"schema_version = 1\n# original contract one after\n".to_vec(),
            },
            FileEdit {
                relative_path: marker::MARKER_FILE_NAME.into(),
                before: None,
                after: marker::previous_marker_bytes(),
            },
        ],
        None,
    )
    .expect("旧契约原计划可准备");
    journal::write_journal(root, &journal_value, true).expect("发布旧 active");
    journal_value
}

/// 先按旧 journal 前滚，再在同一 repository 调用升级到 contract 2，回执保留两份备份与累计数量。
#[test]
fn previous_pending_journal_resumes_then_upgrades_before_returning() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let mut previous = previous_contract_transaction(root.path());
    let interrupted = resume_journal_with_hook(root.path(), &mut previous, |point| {
        if point == FaultPoint::AfterManagedPending(0) {
            Err(AppError::conflict("注入旧契约中断"))
        } else {
            Ok(())
        }
    });
    assert!(interrupted.is_err());
    let repository = FileDataMigrationRepository::new(root.path());
    let report = repository.migrate().expect("旧计划恢复后继续新契约升级");

    assert!(report.resumed);
    assert_eq!(report.outcome, DataMigrationOutcome::Resumed);
    assert_eq!(report.changed_file_count, 3);
    assert_eq!(report.backup_relative_paths.len(), 2);
    assert_eq!(
        report.backup_relative_path.as_ref(),
        report.backup_relative_paths.last()
    );
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("原 after 保留"),
        b"schema_version = 1\n# original contract one after\n",
    );
    let first_backup = root.path().join(&report.backup_relative_paths[0]);
    assert_eq!(
        fs::read(first_backup.join("before/managed/settings.toml")).expect("旧 before 备份保留"),
        b"schema_version = 1\n# before previous migration\n",
    );
    assert!(first_backup.join("committed.json").is_file());
    let second_backup = root.path().join(&report.backup_relative_paths[1]);
    assert_eq!(
        fs::read(second_backup.join("before/managed/.kerminal-format.toml"))
            .expect("旧 marker 原文备份保留"),
        marker::previous_marker_bytes(),
    );
    assert!(matches!(
        marker::read_marker(root.path()).expect("最终 marker 可读"),
        marker::MarkerRead::Current
    ));
    assert_eq!(
        repository.inspect().expect("最终预检可读").readiness,
        DataMigrationReadiness::Current
    );
}

/// 支持旧 marker 的注释和换行精确 CAS，重复执行不能再生成一套迁移备份。
#[test]
fn previous_marker_upgrade_backs_up_exact_source_and_is_idempotent() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let original = b"# previous candidate\r\nmigration_contract = 1\r\ndata_format = 4\r\n";
    fs::write(root.path().join(marker::MARKER_FILE_NAME), original).expect("写入旧 marker");
    let repository = FileDataMigrationRepository::new(root.path());
    let inspection = repository.inspect().expect("旧契约可预检");
    assert_eq!(inspection.readiness, DataMigrationReadiness::Legacy);
    assert_eq!(inspection.migration_contract, Some(1));
    let report = repository.migrate().expect("显式升级旧契约");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    assert_eq!(report.changed_file_count, 1);
    assert_eq!(report.backup_relative_paths.len(), 1);
    assert_eq!(
        fs::read(
            root.path()
                .join(&report.backup_relative_paths[0])
                .join("before/managed/.kerminal-format.toml")
        )
        .expect("精确 marker before 备份"),
        original,
    );
    let repeated = repository.migrate().expect("重复调用只读 current");
    assert_eq!(repeated.outcome, DataMigrationOutcome::AlreadyCurrent);
    assert!(repeated.backup_relative_paths.is_empty());
}

/// 已提交旧契约后的正常用户配置修改不应被历史 after 锁住，升级不能回放覆盖用户新值。
#[test]
fn completed_previous_journal_does_not_overwrite_later_user_edits() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let previous = previous_contract_transaction(root.path());
    resume_journal(root.path(), previous).expect("旧迁移已提交");
    let user_value = b"schema_version = 1\n# user edited after contract one\n";
    fs::write(root.path().join("settings.toml"), user_value).expect("模拟正常用户编辑");
    let repository = FileDataMigrationRepository::new(root.path());
    let report = repository
        .migrate()
        .expect("基于新配置升级，不回放已提交旧计划");
    assert!(!report.resumed);
    assert_eq!(report.changed_file_count, 1);
    assert_eq!(report.backup_relative_paths.len(), 2);
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("用户原文保留"),
        user_value
    );
    assert_eq!(
        repository.inspect().expect("最终契约完整").readiness,
        DataMigrationReadiness::Current
    );
}

/// 当前 marker 不能绕过损坏恢复文档预检，失败保持文件原文且不新建 journal 或备份。
#[test]
fn current_marker_with_invalid_transfer_recovery_is_blocked_without_writes() {
    let root = tempfile::tempdir().expect("创建隔离根");
    fs::write(
        root.path().join(marker::MARKER_FILE_NAME),
        marker::current_marker_bytes(),
    )
    .expect("写入当前 marker");
    fs::create_dir(root.path().join("data")).expect("创建恢复目录");
    let original = b"{\"schemaVersion\":999,\"records\":[]}";
    fs::write(root.path().join("data/transfer-recovery.json"), original).expect("写入未来恢复记录");
    let repository = FileDataMigrationRepository::new(root.path());
    assert_eq!(
        repository.inspect().expect("预检返回阻断").readiness,
        DataMigrationReadiness::Blocked
    );
    assert!(repository.migrate().is_err());
    assert_eq!(
        fs::read(root.path().join("data/transfer-recovery.json")).expect("未来记录保留"),
        original
    );
    assert!(!root.path().join(journal::MIGRATION_DIRECTORY).exists());
}

/// 新备份集合字段是 additive：旧回执仍能解码，空集合不改变既有 console JSON 形状。
#[test]
fn migration_report_backup_collection_is_backward_compatible() {
    let old = r#"{"outcome":"alreadyCurrent","changedFileCount":0,"resumed":false,"backupRelativePath":null}"#;
    let decoded: DataMigrationReport = serde_json::from_str(old).expect("旧回执可解码");
    assert!(decoded.backup_relative_paths.is_empty());
    let encoded = serde_json::to_value(decoded).expect("当前回执可编码");
    assert!(encoded.get("backupRelativePaths").is_none());
    let report = report(
        DataMigrationOutcome::Migrated,
        1,
        false,
        Some(".kerminal-migration/example".to_owned()),
    );
    let encoded = serde_json::to_value(report).expect("有备份回执可编码");
    assert_eq!(encoded["backupRelativePath"], ".kerminal-migration/example");
    assert_eq!(
        encoded["backupRelativePaths"][0],
        ".kerminal-migration/example"
    );
}

/// marker 与 active 的契约不一致不能被快速路径当成功，防止手工换 marker 跳过已发布计划。
#[test]
fn current_marker_cannot_bypass_a_previous_contract_active_journal() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let previous = previous_contract_transaction(root.path());
    resume_journal(root.path(), previous.clone()).expect("旧契约已提交");
    fs::write(
        root.path().join(marker::MARKER_FILE_NAME),
        marker::current_marker_bytes(),
    )
    .expect("注入不匹配的当前 marker");
    let repository = FileDataMigrationRepository::new(root.path());
    assert_eq!(
        repository.inspect().expect("不匹配可预检").readiness,
        DataMigrationReadiness::Blocked
    );
    assert!(repository.migrate().is_err());
    assert_eq!(
        journal::read_active_journal(root.path())
            .expect("旧证据保留")
            .expect("active存在")
            .operation_id,
        previous.operation_id
    );
}

/// 新升级再次中断后只恢复该 active，但回执仍可定位旧收据与新 marker before 两份材料。
#[test]
fn interrupted_second_contract_keeps_the_first_backup_in_the_recovery_report() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let mut previous = previous_contract_transaction(root.path());
    resume_journal(root.path(), previous.clone()).expect("原契约提交完成");
    previous.state = JournalState::Committed;
    journal::archive_committed_journal(root.path(), &previous).expect("保留旧完成收据");
    let next_id = Uuid::new_v4().to_string();
    journal::ensure_directory(
        root.path(),
        &journal::operation_directory(root.path(), &next_id),
    )
    .expect("创建后续操作目录");
    let mut next = prepare_file_journal(
        root.path(),
        &next_id,
        vec![FileEdit {
            relative_path: marker::MARKER_FILE_NAME.into(),
            before: Some(marker::previous_marker_bytes()),
            after: marker::current_marker_bytes(),
        }],
        None,
    )
    .expect("准备新 marker 原计划");
    journal::replace_archived_journal(root.path(), &previous, &next).expect("发布后续 active");
    let result = resume_journal_with_hook(root.path(), &mut next, |point| {
        if point == FaultPoint::AfterMarkerCommit {
            Err(AppError::conflict("注入第二次契约中断"))
        } else {
            Ok(())
        }
    });
    assert!(result.is_err());
    let report = FileDataMigrationRepository::new(root.path())
        .migrate()
        .expect("恢复新 active 并保留完整备份出口");
    assert!(report.resumed);
    assert_eq!(report.changed_file_count, 1);
    assert_eq!(report.backup_relative_paths.len(), 2);
    assert!(
        report
            .backup_relative_paths
            .contains(&backup_relative_path(&previous.operation_id))
    );
    assert_eq!(
        report.backup_relative_path,
        Some(backup_relative_path(&next_id))
    );
}
