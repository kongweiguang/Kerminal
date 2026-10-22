//! 迁移事务的短锁编排、文件 CAS、回读与 marker 最后提交。
//!
//! @author kongweiguang

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use kerminal_application::{
    AppError, AppResult,
    data_migration::{
        DataMigrationInspection, DataMigrationOutcome, DataMigrationReadiness, DataMigrationReport,
    },
};
use uuid::Uuid;

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

use super::{
    FileEdit, inspection,
    journal::{self, FileJournalEntry, JournalState, MigrationJournal},
    marker, operational_state, sqlite,
};

/// 暴露只读 journal scan 给 inspection；当前格式只检查自己的恢复目录，不碰业务事务。
pub(super) fn inspect_journals(root: &Path) -> AppResult<journal::JournalScan> {
    journal::inspect_journals(root)
}

/// 同一根锁内先恢复旧 journal 再重检升级；只有最终契约完整 current 才能放行普通服务。
pub(super) fn apply_locked(
    root: &Path,
    legacy_window_state: &Option<PathBuf>,
    mut inspection_result: DataMigrationInspection,
) -> AppResult<DataMigrationReport> {
    if inspection_result.readiness == DataMigrationReadiness::Blocked {
        return Err(AppError::conflict("用户数据迁移预检存在阻断项"));
    }
    let mut recovered = report(DataMigrationOutcome::AlreadyCurrent, 0, false, None);
    let mut previous_committed = None;
    if let Some(mut active) = journal::read_active_journal(root)? {
        if active.state != JournalState::Committed {
            let changed = changed_file_count(&active) + usize::from(active.sqlite.is_some());
            let backup = backup_relative_path(&active.operation_id);
            resume_journal(root, active.clone())?;
            active.state = JournalState::Committed;
            recovered = report(DataMigrationOutcome::Resumed, changed, true, Some(backup));
        }
        inspection_result = inspection::inspect_root(root, legacy_window_state.as_deref())?;
        if inspection_result.readiness == DataMigrationReadiness::Current
            && !inspection_result.recovery_pending
        {
            return with_retained_backups(root, recovered);
        }
        if inspection_result.readiness == DataMigrationReadiness::Blocked {
            return Err(AppError::conflict("旧迁移已恢复，但新契约预检仍存在阻断项"));
        }
        // 已提交的旧契约可能已被用户正常编辑，不能把历史业务 after 当成当前配置的锁。
        journal::archive_committed_journal(root, &active)?;
        let previous_backup = backup_relative_path(&active.operation_id);
        if !recovered.backup_relative_paths.contains(&previous_backup) {
            recovered.backup_relative_paths.push(previous_backup);
        }
        previous_committed = Some(active);
    }

    let mut migrated = apply_new_transaction(
        root,
        legacy_window_state,
        inspection_result,
        previous_committed.as_ref(),
    )?;
    let final_inspection = inspection::inspect_root(root, legacy_window_state.as_deref())?;
    if final_inspection.readiness != DataMigrationReadiness::Current
        || final_inspection.recovery_pending
    {
        return Err(AppError::outcome_unknown("迁移提交后尚未达到完整当前契约"));
    }
    migrated.changed_file_count += recovered.changed_file_count;
    migrated.resumed |= recovered.resumed;
    if migrated.resumed {
        migrated.outcome = DataMigrationOutcome::Resumed;
    }
    recovered
        .backup_relative_paths
        .append(&mut migrated.backup_relative_paths);
    migrated.backup_relative_paths = recovered.backup_relative_paths;
    with_retained_backups(root, migrated)
}

/// 后续升级本身再中断时，成功恢复回执仍列出先前事务的备份，不只显示当前 active 的目录。
fn with_retained_backups(
    root: &Path,
    mut report: DataMigrationReport,
) -> AppResult<DataMigrationReport> {
    let scan = journal::inspect_journals(root)?;
    if !scan.blockers.is_empty() || scan.recovery_pending {
        return Err(AppError::outcome_unknown("迁移完成后的恢复材料状态不一致"));
    }
    let mut retained = scan
        .archived_operation_ids
        .iter()
        .map(|id| backup_relative_path(id))
        .collect::<Vec<_>>();
    for path in report.backup_relative_paths {
        if !retained.contains(&path) {
            retained.push(path);
        }
    }
    if let Some(path) = report.backup_relative_path.as_ref() {
        retained.retain(|retained| retained != path);
        retained.push(path.clone());
    }
    report.backup_relative_paths = retained;
    Ok(report)
}

/// 新契约使用独立事务和独立 before 备份，不修改旧 journal 已发布的恢复计划。
///
/// @author kongweiguang
fn apply_new_transaction(
    root: &Path,
    legacy_window_state: &Option<PathBuf>,
    inspection_result: DataMigrationInspection,
    previous_committed: Option<&MigrationJournal>,
) -> AppResult<DataMigrationReport> {
    let mut edits = inspection::collect_plan(root, legacy_window_state.as_deref())
        .map_err(|_| AppError::conflict("用户数据不满足当前迁移规则"))?;
    edits.retain(|edit| {
        edit.before.as_deref() != Some(edit.after.as_slice())
            || edit.relative_path == Path::new(operational_state::LEGACY_FORWARDS)
            || edit.relative_path == Path::new("data/port-forwards/definitions.json")
    });
    inspection::validate_edits(root, &edits)
        .map_err(|_| AppError::conflict("迁移文件路径或原始内容已变化"))?;
    let marker_before = marker_before_upgrade(root)?;

    let operation_id = Uuid::new_v4().to_string();
    let operation_dir = journal::operation_directory(root, &operation_id);
    journal::ensure_directory(root, &operation_dir)?;
    let prepared_sqlite = match sqlite::prepare(root, &operation_dir) {
        Ok(prepared) => prepared,
        Err(error) => {
            cleanup_unpublished_operation(root, &operation_id);
            return Err(error);
        }
    };
    edits.push(FileEdit {
        relative_path: PathBuf::from(marker::MARKER_FILE_NAME),
        before: marker_before,
        after: marker::current_marker_bytes(),
    });
    let outcome = match inspection_result.readiness {
        DataMigrationReadiness::Fresh => DataMigrationOutcome::Initialized,
        DataMigrationReadiness::Current | DataMigrationReadiness::Legacy => {
            DataMigrationOutcome::Migrated
        }
        DataMigrationReadiness::Blocked => {
            return Err(AppError::conflict("用户数据迁移预检存在阻断项"));
        }
    };
    apply_prepared_transaction(
        root,
        &operation_id,
        edits,
        prepared_sqlite.journal,
        prepared_sqlite.changed,
        outcome,
        previous_committed,
    )
}

/// A current-contract guide correction uses the same journal, backup, CAS and recovery path while
/// supplying only its frozen guide edit and a same-value marker CAS sentinel.
///
/// @author kongweiguang
pub(super) fn apply_current_guide_locked(
    root: &Path,
    guide_edit: FileEdit,
) -> AppResult<DataMigrationReport> {
    if guide_edit.relative_path != Path::new("kerminal-config.md")
        || guide_edit.before.is_none()
        || guide_edit.after != crate::configuration::managed_config_guide()
    {
        return Err(AppError::conflict("当前格式指南编辑不符合冻结计划"));
    }
    let marker_before = fs::read(root.join(marker::MARKER_FILE_NAME))
        .map_err(|source| AppError::infrastructure_with_source("读取当前格式标记失败", source))?;
    let canonical_marker = marker::current_marker_bytes();
    if marker_before != canonical_marker {
        return Err(AppError::conflict(
            "当前格式标记包含非规范字节；为保留原文，本次未自动更新指南。请手动将配置指南 Settings 示例的六个外观字段移入 [appearance]。",
        ));
    }
    if !matches!(
        marker::classify_marker_bytes(&marker_before)?,
        marker::MarkerRead::Current
    ) {
        return Err(AppError::conflict("指南更新准备时格式标记已变化"));
    }

    let edits = vec![
        guide_edit,
        FileEdit {
            relative_path: PathBuf::from(marker::MARKER_FILE_NAME),
            before: Some(marker_before.clone()),
            after: marker_before,
        },
    ];
    inspection::validate_edits(root, &edits)
        .map_err(|_| AppError::conflict("当前格式指南或标记在预检后发生变化"))?;

    let previous_committed = match journal::read_active_journal(root)? {
        Some(active) if active.state == JournalState::Committed => {
            journal::archive_committed_journal(root, &active)?;
            Some(active)
        }
        Some(_) => return Err(AppError::conflict("当前格式仍有待恢复的迁移事务")),
        None => None,
    };
    let operation_id = Uuid::new_v4().to_string();
    let operation_dir = journal::operation_directory(root, &operation_id);
    journal::ensure_directory(root, &operation_dir)?;
    let report = apply_prepared_transaction(
        root,
        &operation_id,
        edits,
        None,
        false,
        DataMigrationOutcome::Migrated,
        previous_committed.as_ref(),
    )?;
    with_retained_backups(root, report)
}

/// Commit a caller-supplied, already planned edit set through the single durable journal engine.
///
/// @author kongweiguang
fn apply_prepared_transaction(
    root: &Path,
    operation_id: &str,
    edits: Vec<FileEdit>,
    sqlite_entry: Option<sqlite::SqliteJournalEntry>,
    sqlite_changed: bool,
    outcome: DataMigrationOutcome,
    previous_committed: Option<&MigrationJournal>,
) -> AppResult<DataMigrationReport> {
    let journal = match prepare_file_journal(root, operation_id, edits, sqlite_entry) {
        Ok(journal) => journal,
        Err(error) => {
            cleanup_unpublished_operation(root, operation_id);
            return Err(error);
        }
    };
    let publish = match previous_committed {
        Some(previous) => journal::replace_archived_journal(root, previous, &journal),
        None => journal::write_journal(root, &journal, true),
    };
    if let Err(error) = publish {
        return Err(handle_journal_publish_error(root, operation_id, error));
    }

    let changed = changed_file_count(&journal) + usize::from(sqlite_changed);
    resume_journal(root, journal)?;
    Ok(report(
        outcome,
        changed,
        false,
        Some(backup_relative_path(operation_id)),
    ))
}

/// 普通文件仅计算真实内容变化，SQLite 由调用方单独计数，CAS 哨兵不冒充用户文件修改。
fn changed_file_count(journal: &MigrationJournal) -> usize {
    journal
        .files
        .iter()
        .filter(|entry| entry.before_sha256.as_deref() != Some(entry.after_sha256.as_str()))
        .count()
}

/// marker 升级保留源文件精确字节做 CAS；只有已知 (4, 1) 可覆盖，其他标记均拒绝。
fn marker_before_upgrade(root: &Path) -> AppResult<Option<Vec<u8>>> {
    match marker::read_marker(root)? {
        marker::MarkerRead::Missing => Ok(None),
        marker::MarkerRead::PreviousSupported => fs::read(root.join(marker::MARKER_FILE_NAME))
            .map(Some)
            .map_err(|source| AppError::infrastructure_with_source("读取旧格式标记失败", source)),
        marker::MarkerRead::Current | marker::MarkerRead::Blocked(_) => {
            Err(AppError::conflict("迁移准备时格式标记已变化"))
        }
    }
}

/// marker before 接受已知上一契约，或指南专用事务的同值 current 哨兵；其它状态拒绝。
///
/// @author kongweiguang
fn prepare_file_journal(
    root: &Path,
    operation_id: &str,
    edits: Vec<FileEdit>,
    sqlite_entry: Option<sqlite::SqliteJournalEntry>,
) -> AppResult<MigrationJournal> {
    let current_marker_bytes = marker::current_marker_bytes();
    let current_guide_transaction = if sqlite_entry.is_none() && edits.len() == 2 {
        let guide_edit = edits
            .iter()
            .find(|edit| edit.relative_path == Path::new("kerminal-config.md"));
        let marker_edit = edits
            .iter()
            .find(|edit| edit.relative_path == Path::new(marker::MARKER_FILE_NAME));
        match (
            guide_edit,
            marker_edit,
            super::configuration::plan_current_config_guide_edit(root)?,
        ) {
            (Some(guide_edit), Some(marker_edit), Some(planned)) => {
                guide_edit.before.as_deref() == planned.before.as_deref()
                    && guide_edit.after.as_slice() == planned.after.as_slice()
                    && marker_edit.before.as_deref() == Some(current_marker_bytes.as_slice())
                    && marker_edit.after.as_slice() == current_marker_bytes.as_slice()
            }
            _ => false,
        }
    } else {
        false
    };
    let operation_dir = journal::operation_directory(root, operation_id);
    let mut entries = Vec::with_capacity(edits.len());
    for edit in edits {
        let entry = journal::file_entry(&edit);
        let supported_marker_before = match edit.before.as_deref() {
            None => true,
            Some(bytes) => {
                matches!(
                    marker::classify_marker_bytes(bytes),
                    Ok(marker::MarkerRead::PreviousSupported)
                ) || current_guide_transaction
                    && bytes == current_marker_bytes.as_slice()
                    && edit.before.as_deref() == Some(edit.after.as_slice())
            }
        };
        if !journal::is_managed_path(&entry.relative_path)
            || entry.relative_path == marker::MARKER_FILE_NAME && !supported_marker_before
        {
            return Err(AppError::conflict("迁移 planner 返回了不安全的文件路径"));
        }
        ensure_file_state(root, &entry, false)?;
        let stage = staged_path(&operation_dir, &entry.relative_path)?;
        journal::write_new_file(&stage, &edit.after)?;
        if let Some(before) = edit.before.as_deref() {
            let backup = backup_path(&operation_dir, &entry.relative_path)?;
            journal::write_new_file(&backup, before)?;
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    if let Some(index) = entries
        .iter()
        .position(|entry| entry.relative_path == marker::MARKER_FILE_NAME)
    {
        let marker_entry = entries.remove(index);
        entries.push(marker_entry);
    }
    let journal = MigrationJournal {
        schema_version: 1,
        operation_id: operation_id.to_owned(),
        state: JournalState::Prepared,
        files: entries,
        sqlite: sqlite_entry,
    };
    if !journal::validate_journal(&journal) {
        return Err(AppError::conflict("迁移 planner 产生的事务摘要无效"));
    }
    Ok(journal)
}

/// 以 journal 原文为唯一恢复计划，SQLite、普通文件、完整回读和 marker 依序前滚。
fn resume_journal(root: &Path, mut journal_value: MigrationJournal) -> AppResult<()> {
    resume_journal_with_hook(root, &mut journal_value, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultPoint {
    BeforeSqliteCommit,
    AfterSqliteCommit,
    AfterManagedPending(usize),
    AfterManagedFile(usize),
    AfterManagedReadback,
    AfterMarkerCommit,
}

/// 故障注入只用于验证恢复闭环；生产调用传入恒成功钩子并复用同一 CAS 状态机。
fn resume_journal_with_hook(
    root: &Path,
    journal_value: &mut MigrationJournal,
    mut fault: impl FnMut(FaultPoint) -> AppResult<()>,
) -> AppResult<()> {
    if !journal::validate_journal(journal_value) {
        return Err(AppError::conflict("迁移 journal 无法安全恢复"));
    }
    let operation_dir = journal::operation_directory(root, &journal_value.operation_id);
    journal::require_operation_directory(root, &journal_value.operation_id)?;
    verify_marker_before(root, &operation_dir, journal_value)?;
    if journal_value.state == JournalState::Committed {
        return verify_committed_journal(root, &operation_dir, journal_value);
    }
    if journal_value.state == JournalState::Prepared {
        journal_value.state = JournalState::Committing;
        journal::write_journal(root, journal_value, false)?;
    }
    fault(FaultPoint::BeforeSqliteCommit)?;

    if let Some(sqlite_entry) = journal_value.sqlite.as_ref() {
        match sqlite::inspect_entry(root, &operation_dir, sqlite_entry)? {
            sqlite::SqliteRecoveryState::Committed => {
                sqlite::verify_committed(root, &operation_dir, sqlite_entry)?
            }
            sqlite::SqliteRecoveryState::Prepared
            | sqlite::SqliteRecoveryState::CommitInterrupted => {
                sqlite::apply(root, &operation_dir, sqlite_entry)?;
                sqlite::verify_committed(root, &operation_dir, sqlite_entry)?;
            }
        }
    }
    fault(FaultPoint::AfterSqliteCommit)?;

    for (index, entry) in journal_value.files[..journal_value.files.len() - 1]
        .iter()
        .enumerate()
    {
        apply_file_entry(
            root,
            &operation_dir,
            &journal_value.operation_id,
            index,
            entry,
            &mut fault,
        )?;
        fault(FaultPoint::AfterManagedFile(index))?;
    }
    verify_files_before_marker(root, &operation_dir, journal_value)?;
    fault(FaultPoint::AfterManagedReadback)?;
    let marker_index = journal_value.files.len() - 1;
    apply_file_entry(
        root,
        &operation_dir,
        &journal_value.operation_id,
        marker_index,
        &journal_value.files[marker_index],
        &mut fault,
    )?;
    if !marker_matches_journal(root, journal_value)? {
        return Err(AppError::outcome_unknown("迁移格式标记提交后回读失败"));
    }
    fault(FaultPoint::AfterMarkerCommit)?;
    journal_value.state = JournalState::Committed;
    journal::write_journal(root, journal_value, false)
}

/// 已提交 journal 复入按其原 marker 摘要回读；旧契约的收敛不冒充本次升级完成。
fn verify_committed_journal(
    root: &Path,
    operation_dir: &Path,
    journal_value: &MigrationJournal,
) -> AppResult<()> {
    if !marker_matches_journal(root, journal_value)? {
        return Err(AppError::conflict(
            "已提交迁移 journal 与数据格式标记不一致",
        ));
    }
    for entry in &journal_value.files {
        let target = checked_target_path(root, &entry.relative_path)?;
        if file_hash_or_missing(&target)?.as_deref() != Some(entry.after_sha256.as_str()) {
            return Err(AppError::conflict("已提交迁移文件与 journal 摘要不一致"));
        }
    }
    if let Some(entry) = journal_value.sqlite.as_ref() {
        sqlite::verify_committed(root, operation_dir, entry)?;
    }
    Ok(())
}

/// 已发布 journal 只能写原契约 canonical after，不接受碰巧可解析但摘要不同的 marker。
fn marker_matches_journal(root: &Path, journal_value: &MigrationJournal) -> AppResult<bool> {
    let Some(entry) = journal_value.files.last() else {
        return Ok(false);
    };
    let state = marker::read_marker(root)?;
    if !matches!(
        state,
        marker::MarkerRead::Current | marker::MarkerRead::PreviousSupported
    ) {
        return Ok(false);
    }
    let target = checked_target_path(root, marker::MARKER_FILE_NAME)?;
    Ok(file_hash_or_missing(&target)?.as_deref() == Some(entry.after_sha256.as_str()))
}

/// before 摘要本身不能证明版本；恢复核对旧契约 marker，或校验 current no-op 哨兵的同值摘要。
///
/// @author kongweiguang
fn verify_marker_before(
    root: &Path,
    operation_dir: &Path,
    journal_value: &MigrationJournal,
) -> AppResult<()> {
    let Some(entry) = journal_value.files.last() else {
        return Err(AppError::conflict("迁移 journal 缺少格式标记"));
    };
    if entry.before_sha256.is_none() {
        return Ok(());
    }
    ensure_backup(root, operation_dir, entry)?;
    let bytes = fs::read(backup_path(operation_dir, marker::MARKER_FILE_NAME)?)
        .map_err(|source| AppError::infrastructure_with_source("读取旧格式标记备份失败", source))?;
    let marker_state = marker::classify_marker_bytes(&bytes)?;
    let current_marker_hash = journal::sha256(&marker::current_marker_bytes());
    let current_guide_hash = journal::sha256(&crate::configuration::managed_config_guide());
    let guide_entry = journal_value
        .files
        .iter()
        .find(|candidate| candidate.relative_path == "kerminal-config.md");
    let same_current_guide_transaction = matches!(&marker_state, marker::MarkerRead::Current)
        && journal_value.sqlite.is_none()
        && journal_value.files.len() == 2
        && entry.before_sha256.as_deref() == Some(current_marker_hash.as_str())
        && entry.after_sha256 == current_marker_hash
        && guide_entry.is_some_and(|guide| {
            guide.before_sha256.is_some()
                && guide.before_sha256.as_deref() != Some(guide.after_sha256.as_str())
                && guide.after_sha256 == current_guide_hash
        });
    // 冻结正文仍须绑定 journal 的 before 摘要，且沿现有私有路径检查拒绝链接备份。
    let frozen_guide_backup = if let Some(guide) =
        guide_entry.filter(|_| same_current_guide_transaction)
    {
        ensure_backup(root, operation_dir, guide)?;
        let guide_before = fs::read(backup_path(operation_dir, &guide.relative_path)?)
            .map_err(|source| AppError::infrastructure_with_source("读取旧指南备份失败", source))?;
        super::configuration::is_frozen_config_guide_bytes(&guide_before)
    } else {
        false
    };
    if !matches!(&marker_state, marker::MarkerRead::PreviousSupported) && !frozen_guide_backup {
        return Err(AppError::conflict("格式标记 before 不是支持的上一契约"));
    }
    Ok(())
}

/// 对 managed 文件执行 before/after CAS；唯一 pending sibling 支持替换前中断后的确定性恢复。
fn apply_file_entry(
    root: &Path,
    operation_dir: &Path,
    operation_id: &str,
    index: usize,
    entry: &FileJournalEntry,
    fault: &mut impl FnMut(FaultPoint) -> AppResult<()>,
) -> AppResult<()> {
    let target = checked_target_path(root, &entry.relative_path)?;
    let stage = staged_path(operation_dir, &entry.relative_path)?;
    let pending = pending_path(&target, operation_id, index)?;
    journal::ensure_directory(
        root,
        target
            .parent()
            .ok_or_else(|| AppError::invalid("迁移目标没有父目录"))?,
    )?;

    let current = file_hash_or_missing(&target)?;
    if current.as_deref() == Some(entry.after_sha256.as_str()) {
        if pending_hash(root, &target, &pending)?.is_some() {
            return Err(AppError::conflict("迁移 pending 与已提交目标同时存在"));
        }
        return Ok(());
    }
    if current != entry.before_sha256 {
        return Err(AppError::conflict(
            "迁移目标内容与 journal before/after 均不匹配",
        ));
    }
    ensure_backup(root, operation_dir, entry)?;

    if pending_hash(root, &target, &pending)?.is_none() {
        let staged_hash = private_file_hash(root, operation_dir, &stage)?;
        if staged_hash.as_deref() != Some(entry.after_sha256.as_str()) {
            return Err(AppError::conflict("迁移 staging 缺失或摘要不匹配"));
        }
        match commit_staged_file(&stage, &pending, CommitMode::MoveStagedNoReplace) {
            Ok(()) => {}
            Err(AtomicCommitError::CommitUnknown(source)) => {
                if pending_hash(root, &target, &pending)?.as_deref()
                    != Some(entry.after_sha256.as_str())
                {
                    return Err(AppError::outcome_unknown_with_source(
                        "迁移 pending 移动结果未知",
                        source,
                    ));
                }
            }
            Err(AtomicCommitError::NotCommitted(source)) => {
                return Err(AppError::infrastructure_with_source(
                    "迁移 pending 准备失败",
                    source,
                ));
            }
        }
    } else if private_file_hash(root, operation_dir, &stage)?.is_some() {
        return Err(AppError::conflict("迁移 staging 与 pending 同时存在"));
    }
    if pending_hash(root, &target, &pending)?.as_deref() != Some(entry.after_sha256.as_str()) {
        return Err(AppError::conflict("迁移 pending 内容与目标摘要不匹配"));
    }
    fault(FaultPoint::AfterManagedPending(index))?;
    if file_hash_or_missing(&target)? != entry.before_sha256 {
        return Err(AppError::conflict("迁移目标在原子提交前发生变化"));
    }
    let mode = if entry.before_sha256.is_some() {
        CommitMode::ReplaceExisting
    } else {
        CommitMode::MoveNoReplace
    };
    match commit_staged_file(&pending, &target, mode) {
        Ok(()) => {}
        Err(AtomicCommitError::CommitUnknown(source)) => {
            if file_hash_or_missing(&target)?.as_deref() != Some(entry.after_sha256.as_str()) {
                return Err(AppError::outcome_unknown_with_source(
                    "迁移文件原子提交结果未知",
                    source,
                ));
            }
        }
        Err(AtomicCommitError::NotCommitted(source)) => {
            return Err(AppError::infrastructure_with_source(
                "迁移文件原子提交失败",
                source,
            ));
        }
    }
    if file_hash_or_missing(&target)?.as_deref() != Some(entry.after_sha256.as_str()) {
        return Err(AppError::outcome_unknown("迁移文件提交后回读摘要不匹配"));
    }
    Ok(())
}

/// marker 提交前重新读取每个业务目标及 before 备份，杜绝 partial write 被发布为当前格式。
fn verify_files_before_marker(
    root: &Path,
    operation_dir: &Path,
    journal_value: &MigrationJournal,
) -> AppResult<()> {
    for entry in &journal_value.files[..journal_value.files.len() - 1] {
        let target = checked_target_path(root, &entry.relative_path)?;
        if file_hash_or_missing(&target)?.as_deref() != Some(entry.after_sha256.as_str()) {
            return Err(AppError::outcome_unknown(
                "迁移业务文件完整回读失败，格式标记未提交",
            ));
        }
        ensure_backup(root, operation_dir, entry)?;
    }
    if let Some(entry) = journal_value.sqlite.as_ref() {
        sqlite::verify_committed(root, operation_dir, entry)?;
    }
    Ok(())
}

/// before 存在时必须同时保留精确字节备份；未知或被替换的备份不能继续提交。
fn ensure_backup(root: &Path, operation_dir: &Path, entry: &FileJournalEntry) -> AppResult<()> {
    let Some(expected) = entry.before_sha256.as_deref() else {
        return Ok(());
    };
    let backup = backup_path(operation_dir, &entry.relative_path)?;
    if private_file_hash(root, operation_dir, &backup)?.as_deref() != Some(expected) {
        return Err(AppError::conflict("迁移 before 备份缺失或摘要不匹配"));
    }
    Ok(())
}

/// 准备阶段及每次提交前校验原文件，拒绝任何 planner 之后发生的外部修改。
fn ensure_file_state(root: &Path, entry: &FileJournalEntry, allow_after: bool) -> AppResult<()> {
    let target = checked_target_path(root, &entry.relative_path)?;
    let current = file_hash_or_missing(&target)?;
    if current == entry.before_sha256
        || allow_after && current.as_deref() == Some(entry.after_sha256.as_str())
    {
        Ok(())
    } else {
        Err(AppError::conflict("迁移源文件在预检后发生变化"))
    }
}

/// 按固定相对路径解析目标，逐级检查根、父目录和末端，任何链接都阻止读写。
fn checked_target_path(root: &Path, relative: &str) -> AppResult<PathBuf> {
    if !journal::is_managed_path(relative) {
        return Err(AppError::conflict("迁移目标路径不在受管范围内"));
    }
    let mut path = root.to_path_buf();
    let components = Path::new(relative).components().collect::<Vec<_>>();
    if fs::symlink_metadata(root)
        .map_err(|source| AppError::infrastructure_with_source("检查迁移数据根失败", source))?
        .file_type()
        .is_symlink()
    {
        return Err(AppError::conflict("迁移数据根不能是链接"));
    }
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            return Err(AppError::conflict("迁移目标路径包含非普通段"));
        };
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::conflict("迁移目标路径包含符号链接"));
            }
            Ok(metadata) if index + 1 == components.len() && !metadata.is_file() => {
                return Err(AppError::conflict("迁移目标不是普通文件"));
            }
            Ok(metadata) if index + 1 != components.len() && !metadata.is_dir() => {
                return Err(AppError::conflict("迁移目标父路径不是目录"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "检查迁移目标路径失败",
                    source,
                ));
            }
        }
    }
    Ok(root.join(relative))
}

/// 哈希普通文件并拒绝链接；缺失末端返回 None，用于判断新建文件的 CAS 状态。
fn file_hash_or_missing(path: &Path) -> AppResult<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(AppError::conflict("迁移文件路径不是普通文件"))
        }
        Ok(_) => {
            let bytes = fs::read(path).map_err(|source| {
                AppError::infrastructure_with_source("读取迁移文件失败", source)
            })?;
            Ok(Some(journal::sha256(&bytes)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(AppError::infrastructure_with_source(
            "读取迁移文件状态失败",
            source,
        )),
    }
}

/// 验证目标父目录仍是根内普通目录后再读取同目录 pending，拒绝经链接读取外部文件。
fn pending_hash(root: &Path, target: &Path, pending: &Path) -> AppResult<Option<String>> {
    if pending.parent() != target.parent() {
        return Err(AppError::conflict("迁移 pending 与目标不在同一目录"));
    }
    let relative = target
        .strip_prefix(root)
        .map_err(|_| AppError::conflict("迁移 pending 越过数据根"))?;
    let components = relative.components().collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for component in &components[..components.len().saturating_sub(1)] {
        let Component::Normal(part) = component else {
            return Err(AppError::conflict("迁移 pending 父路径无效"));
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current).map_err(|source| {
            AppError::infrastructure_with_source("检查迁移 pending 目录失败", source)
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::conflict("迁移 pending 父路径包含链接或非目录项"));
        }
    }
    file_hash_or_missing(pending)
}

/// 只在 operation UUID 内哈希备份或 staging，并验证其全部父目录都未被替换为链接。
fn private_file_hash(root: &Path, operation_dir: &Path, path: &Path) -> AppResult<Option<String>> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| {
        AppError::infrastructure_with_source("检查迁移 private 数据根失败", source)
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(AppError::conflict("迁移 private 数据根不是普通目录"));
    }
    let operation_relative = operation_dir
        .strip_prefix(root)
        .map_err(|_| AppError::conflict("迁移 private 目录越过数据根"))?;
    let operation_parts = operation_relative.components().collect::<Vec<_>>();
    let [
        Component::Normal(directory),
        Component::Normal(operation_id),
    ] = operation_parts.as_slice()
    else {
        return Err(AppError::conflict("迁移 private 目录结构无效"));
    };
    if directory.to_str() != Some(journal::MIGRATION_DIRECTORY)
        || Uuid::parse_str(&operation_id.to_string_lossy()).is_err()
        || journal::operation_directory(root, &operation_id.to_string_lossy()) != operation_dir
    {
        return Err(AppError::conflict("迁移 private 目录不属于本次操作"));
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| AppError::conflict("迁移 private 文件越过数据根"))?;
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(AppError::conflict("迁移 private 文件路径无效"));
    }
    let mut current = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            return Err(AppError::conflict("迁移 private 文件路径无效"));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::conflict("迁移 private 路径包含链接"));
            }
            Ok(metadata) if index + 1 == components.len() && !metadata.is_file() => {
                return Err(AppError::conflict("迁移 private 文件不是普通文件"));
            }
            Ok(metadata) if index + 1 != components.len() && !metadata.is_dir() => {
                return Err(AppError::conflict("迁移 private 父路径不是目录"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "读取迁移 private 文件状态失败",
                    source,
                ));
            }
        }
    }
    file_hash_or_missing(path)
}

/// 对固定 private 子目录生成 staging 路径，relative 已由 allowlist 拒绝点段和反斜杠。
fn staged_path(operation_dir: &Path, relative: &str) -> AppResult<PathBuf> {
    private_relative_path(operation_dir, Path::new("staged/managed").join(relative))
}

/// 对固定 private 子目录生成 before 备份路径。
fn backup_path(operation_dir: &Path, relative: &str) -> AppResult<PathBuf> {
    private_relative_path(operation_dir, Path::new("before/managed").join(relative))
}

/// 组合 private 路径并保证每一层都位于 operation_dir 之内。
fn private_relative_path(operation_dir: &Path, relative: PathBuf) -> AppResult<PathBuf> {
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(AppError::conflict("迁移 private 文件路径无效"));
    }
    Ok(operation_dir.join(relative))
}

/// 生成同目录确定性 pending，保证 ReplaceExisting 满足平台原子替换 API 的同目录约束。
fn pending_path(target: &Path, operation_id: &str, index: usize) -> AppResult<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| AppError::invalid("迁移目标没有文件名"))?;
    Ok(target.with_file_name(format!(
        ".{}.kerminal-{operation_id}-{index}.pending",
        name.to_string_lossy()
    )))
}

/// 返回包含 managed 与 SQLite before、stage 和 journal 的完整恢复目录。
fn backup_relative_path(operation_id: &str) -> String {
    format!("{}/{operation_id}", journal::MIGRATION_DIRECTORY)
}

/// 迁移准备失败时只清理本次新建且尚未发布 journal 的 UUID private 目录。
fn cleanup_unpublished_operation(root: &Path, operation_id: &str) {
    let Ok(parsed_id) = Uuid::parse_str(operation_id) else {
        return;
    };
    if parsed_id.hyphenated().to_string() != operation_id {
        return;
    }
    let migration_root = root.join(journal::MIGRATION_DIRECTORY);
    let operation_dir = journal::operation_directory(root, operation_id);
    if operation_dir.parent() != Some(migration_root.as_path()) {
        return;
    }
    for path in [root, migration_root.as_path(), operation_dir.as_path()] {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return;
        }
    }
    let _ = fs::remove_dir_all(operation_dir);
}

/// 只有确定未发布时清理本次 UUID；未知提交必须保留 active 引用的恢复材料。
fn handle_journal_publish_error(root: &Path, operation_id: &str, error: AppError) -> AppError {
    if !matches!(error, AppError::OutcomeUnknown { .. }) {
        cleanup_unpublished_operation(root, operation_id);
    }
    error
}

/// 单事务回执同时保留兼容单路径与完整路径集合，连续升级由调用层按执行顺序累加。
fn report(
    outcome: DataMigrationOutcome,
    changed_file_count: usize,
    resumed: bool,
    backup_relative_path: Option<String>,
) -> DataMigrationReport {
    DataMigrationReport {
        outcome,
        changed_file_count,
        resumed,
        backup_relative_paths: backup_relative_path.iter().cloned().collect(),
        backup_relative_path,
    }
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_transaction_unit.rs"]
mod tests;
