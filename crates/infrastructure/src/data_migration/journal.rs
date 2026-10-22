//! 迁移 journal 的受控格式、路径边界与原子读写。
//!
//! @author kongweiguang

use std::{
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use kerminal_application::{AppError, AppResult, data_migration::DataMigrationBlocker};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

use super::{FileEdit, marker, sqlite};

pub(super) const MIGRATION_DIRECTORY: &str = ".kerminal-migration";
const ACTIVE_JOURNAL: &str = "active.json";
const COMMITTED_RECEIPT: &str = "committed.json";
const JOURNAL_SCHEMA: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;

/// 描述一次只前滚的迁移；目标与备份路径由 operation_id 和固定相对路径派生。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MigrationJournal {
    pub schema_version: u32,
    pub operation_id: String,
    pub state: JournalState,
    pub files: Vec<FileJournalEntry>,
    pub sqlite: Option<sqlite::SqliteJournalEntry>,
}

/// 迁移状态只允许预备、提交中和已提交，未知阶段绝不猜测恢复动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum JournalState {
    Prepared,
    Committing,
    Committed,
}

/// 一个固定受管文件的 before/after 内容摘要；正文只存放在隔离 staging 与 backup 文件。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileJournalEntry {
    pub relative_path: String,
    pub before_sha256: Option<String>,
    pub after_sha256: String,
}

/// 迁移根扫描结果只包含受控状态，不包含用户路径或文件内容。
#[derive(Default)]
pub(super) struct JournalScan {
    pub blockers: Vec<DataMigrationBlocker>,
    pub recovery_pending: bool,
    pub committed: bool,
    pub target_migration_contract: Option<u32>,
    pub archived_operation_ids: Vec<String>,
}

/// 只接受唯一 active 与有不可变完成收据的历史 UUID 目录，保留连续契约升级的所有备份。
pub(super) fn inspect_journals(root: &Path) -> AppResult<JournalScan> {
    let migration_root = root.join(MIGRATION_DIRECTORY);
    let metadata = match fs::symlink_metadata(&migration_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(JournalScan::default());
        }
        Err(source) => return Err(io_error("读取迁移恢复目录失败", source)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(blocked_scan(DataMigrationBlocker::CorruptMigrationJournal));
    }

    let mut operation_ids = std::collections::BTreeSet::new();
    for item in
        fs::read_dir(&migration_root).map_err(|source| io_error("读取迁移恢复目录失败", source))?
    {
        let item = item.map_err(|source| io_error("读取迁移恢复目录失败", source))?;
        let name = item.file_name();
        if name == ACTIVE_JOURNAL {
            let file_type = item
                .file_type()
                .map_err(|source| io_error("读取迁移 journal 类型失败", source))?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Ok(blocked_scan(DataMigrationBlocker::CorruptMigrationJournal));
            }
            continue;
        }
        let file_type = item
            .file_type()
            .map_err(|source| io_error("读取迁移恢复目录失败", source))?;
        if !file_type.is_dir() || Uuid::parse_str(&name.to_string_lossy()).is_err() {
            return Ok(blocked_scan(DataMigrationBlocker::OrphanedMigrationData));
        }
        operation_ids.insert(name.to_string_lossy().into_owned());
    }

    match read_active_journal(root) {
        Err(_) => Ok(blocked_scan(DataMigrationBlocker::CorruptMigrationJournal)),
        Ok(Some(journal)) if operation_ids.contains(&journal.operation_id) => {
            if require_operation_directory(root, &journal.operation_id).is_err()
                || operation_ids.iter().any(|id| {
                    if id == &journal.operation_id {
                        match read_committed_receipt(root, id) {
                            Ok(None) => false,
                            Ok(Some(receipt)) => !same_journal(&receipt, &journal),
                            Err(_) => true,
                        }
                    } else {
                        !matches!(read_committed_receipt(root, id), Ok(Some(_)))
                    }
                })
            {
                return Ok(blocked_scan(DataMigrationBlocker::CorruptMigrationJournal));
            }
            Ok(JournalScan {
                blockers: Vec::new(),
                recovery_pending: journal.state != JournalState::Committed,
                committed: journal.state == JournalState::Committed,
                target_migration_contract: journal.files.last().map(|entry| {
                    if entry.after_sha256 == sha256(&marker::previous_marker_bytes()) {
                        marker::PREVIOUS_MIGRATION_CONTRACT
                    } else {
                        marker::CURRENT_MIGRATION_CONTRACT
                    }
                }),
                archived_operation_ids: operation_ids
                    .into_iter()
                    .filter(|id| id != &journal.operation_id)
                    .collect(),
            })
        }
        Ok(Some(_)) => Ok(blocked_scan(DataMigrationBlocker::CorruptMigrationJournal)),
        Ok(None) if !operation_ids.is_empty() => {
            Ok(blocked_scan(DataMigrationBlocker::OrphanedMigrationData))
        }
        Ok(None) => Ok(JournalScan::default()),
    }
}

/// 在替换 active 之前原子保存原 journal；失败时保留原 active 与全部恢复材料而不删除备份。
pub(super) fn archive_committed_journal(root: &Path, journal: &MigrationJournal) -> AppResult<()> {
    if journal.state != JournalState::Committed || !validate_journal(journal) {
        return Err(AppError::conflict("只能归档已完成的迁移 journal"));
    }
    if let Some(receipt) = read_committed_receipt(root, &journal.operation_id)? {
        return if same_journal(&receipt, journal) {
            Ok(())
        } else {
            Err(AppError::conflict("迁移完成收据与 active 内容冲突"))
        };
    }
    let directory = require_operation_directory(root, &journal.operation_id)?;
    let target = directory.join(COMMITTED_RECEIPT);
    let stage = directory.join(format!(".committed-{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec(journal)
        .map_err(|_| AppError::infrastructure("编码迁移完成收据失败"))?;
    if let Err(error) = write_new_file(&stage, &bytes) {
        remove_owned_stage(&stage);
        return Err(error);
    }
    match commit_staged_file(&stage, &target, CommitMode::CreateNew) {
        Ok(()) => {}
        Err(AtomicCommitError::CommitUnknown(source)) => {
            if !matches!(
                read_committed_receipt(root, &journal.operation_id),
                Ok(Some(ref actual)) if same_journal(actual, journal)
            ) {
                return Err(AppError::outcome_unknown_with_source(
                    "迁移完成收据发布结果未知",
                    source,
                ));
            }
        }
        Err(AtomicCommitError::NotCommitted(source)) => {
            remove_owned_stage(&stage);
            return Err(io_error("发布迁移完成收据失败", source));
        }
    }
    if !matches!(
        read_committed_receipt(root, &journal.operation_id),
        Ok(Some(ref actual)) if same_journal(actual, journal)
    ) {
        return Err(AppError::outcome_unknown("迁移完成收据发布后回读失败"));
    }
    Ok(())
}

/// 只有原 active 与不可变收据均匹配才替换；根锁外的第三值不被新事务误覆盖。
pub(super) fn replace_archived_journal(
    root: &Path,
    previous: &MigrationJournal,
    next: &MigrationJournal,
) -> AppResult<()> {
    if previous.state != JournalState::Committed
        || !matches!(read_active_journal(root), Ok(Some(ref active)) if same_journal(active, previous))
        || !matches!(
            read_committed_receipt(root, &previous.operation_id),
            Ok(Some(ref receipt)) if same_journal(receipt, previous)
        )
    {
        return Err(AppError::conflict("迁移 active 或完成收据已变化"));
    }
    write_journal(root, next, false)
}

/// 历史收据只证明备份的原事务归属，不回放或用旧 after 覆盖后续升级后的业务文件。
fn read_committed_receipt(root: &Path, operation_id: &str) -> AppResult<Option<MigrationJournal>> {
    let directory = require_operation_directory(root, operation_id)?;
    let path = directory.join(COMMITTED_RECEIPT);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("读取迁移完成收据失败", source)),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_JOURNAL_BYTES
    {
        return Err(AppError::conflict("迁移完成收据不是安全普通文件"));
    }
    let bytes = fs::read(path).map_err(|source| io_error("读取迁移完成收据失败", source))?;
    let journal: MigrationJournal =
        serde_json::from_slice(&bytes).map_err(|_| AppError::conflict("迁移完成收据内容损坏"))?;
    if journal.operation_id != operation_id
        || journal.state != JournalState::Committed
        || !validate_journal(&journal)
    {
        return Err(AppError::conflict("迁移完成收据与操作目录不匹配"));
    }
    Ok(Some(journal))
}

/// 读取 active journal 并校验全部路径、摘要和 SQLite 字段后才允许其驱动恢复。
pub(super) fn read_active_journal(root: &Path) -> AppResult<Option<MigrationJournal>> {
    let path = root.join(MIGRATION_DIRECTORY).join(ACTIVE_JOURNAL);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("读取迁移 journal 失败", source)),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_JOURNAL_BYTES
    {
        return Err(AppError::conflict("迁移 journal 路径不是安全普通文件"));
    }
    let bytes = fs::read(path).map_err(|source| io_error("读取迁移 journal 失败", source))?;
    let journal = match serde_json::from_slice::<MigrationJournal>(&bytes) {
        Ok(journal) if validate_journal(&journal) => journal,
        _ => return Err(AppError::conflict("迁移 journal 无法安全恢复")),
    };
    Ok(Some(journal))
}

/// 首次以 CreateNew 发布，后续状态改用同目录原子替换，并回读处理未知提交结果。
pub(super) fn write_journal(
    root: &Path,
    journal: &MigrationJournal,
    create: bool,
) -> AppResult<()> {
    write_journal_with_commit(root, journal, create, commit_staged_file)
}

/// 把原子替换作为边界参数，生产复用存储 adapter，测试可精确注入提交阶段故障。
fn write_journal_with_commit(
    root: &Path,
    journal: &MigrationJournal,
    create: bool,
    commit: impl FnOnce(&Path, &Path, CommitMode) -> Result<(), AtomicCommitError>,
) -> AppResult<()> {
    if !validate_journal(journal) {
        return Err(AppError::conflict("迁移 journal 内容不符合当前格式"));
    }
    let directory = root.join(MIGRATION_DIRECTORY);
    ensure_directory(root, &directory)?;
    let target = directory.join(ACTIVE_JOURNAL);
    let stage = directory.join(format!(".active-{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec(journal)
        .map_err(|_| AppError::infrastructure("编码迁移 journal 失败"))?;
    if let Err(error) = write_new_file(&stage, &bytes) {
        remove_owned_stage(&stage);
        return Err(error);
    }
    let mode = if create {
        CommitMode::CreateNew
    } else {
        CommitMode::ReplaceExisting
    };
    match commit(&stage, &target, mode) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::CommitUnknown(source)) => {
            let committed = matches!(
                read_active_journal(root),
                Ok(Some(actual)) if same_journal(&actual, journal)
            );
            if committed {
                Ok(())
            } else {
                Err(AppError::outcome_unknown_with_source(
                    "迁移 journal 提交结果未知",
                    source,
                ))
            }
        }
        Err(AtomicCommitError::NotCommitted(source)) => {
            remove_owned_stage(&stage);
            Err(AppError::infrastructure_with_source(
                "迁移 journal 提交失败",
                source,
            ))
        }
    }
}

/// 在固定 UUID 目录内构造本次 staging 与 before 路径。
pub(super) fn operation_directory(root: &Path, operation_id: &str) -> PathBuf {
    root.join(MIGRATION_DIRECTORY).join(operation_id)
}

/// 恢复前只验证 journal 指向的 UUID 目录已存在且无链接，不为丢失的证据新建空目录。
pub(super) fn require_operation_directory(root: &Path, operation_id: &str) -> AppResult<PathBuf> {
    let parsed =
        Uuid::parse_str(operation_id).map_err(|_| AppError::conflict("迁移操作目录 ID 无效"))?;
    if parsed.hyphenated().to_string() != operation_id {
        return Err(AppError::conflict("迁移操作目录 ID 格式无效"));
    }
    let path = operation_directory(root, operation_id);
    let migration_root = root.join(MIGRATION_DIRECTORY);
    for directory in [root, migration_root.as_path(), path.as_path()] {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(AppError::conflict("迁移恢复目录或其父目录缺失"));
            }
            Err(source) => return Err(io_error("检查迁移操作目录失败", source)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::conflict("迁移操作目录缺失或包含链接"));
        }
    }
    Ok(path)
}

/// 保留已发布 contract 1 的精确摘要以便原计划前滚，contract 2 才允许有旧 marker before。
pub(super) fn validate_journal(journal: &MigrationJournal) -> bool {
    let operation_id = Uuid::parse_str(&journal.operation_id);
    if journal.schema_version != JOURNAL_SCHEMA
        || operation_id.is_err()
        || operation_id.is_ok_and(|id| id.hyphenated().to_string() != journal.operation_id)
        || journal.files.is_empty()
        || journal.files.len() > 4096
    {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    for entry in &journal.files {
        if !is_managed_path(&entry.relative_path)
            || !seen.insert(entry.relative_path.as_str())
            || !valid_hash(&entry.after_sha256)
            || (entry.relative_path == "data/port-forwards/sessions.json"
                && entry.before_sha256.as_deref() != Some(entry.after_sha256.as_str()))
            || entry
                .before_sha256
                .as_deref()
                .is_some_and(|hash| !valid_hash(hash))
        {
            return false;
        }
    }
    let Some(marker_entry) = journal.files.last() else {
        return false;
    };
    let current_after = marker_entry.after_sha256 == sha256(&marker::current_marker_bytes());
    let previous_after = marker_entry.after_sha256 == sha256(&marker::previous_marker_bytes());
    if marker_entry.relative_path != marker::MARKER_FILE_NAME
        || !current_after && !previous_after
        || previous_after && marker_entry.before_sha256.is_some()
    {
        return false;
    }
    if journal.files[..journal.files.len() - 1]
        .iter()
        .any(|entry| entry.relative_path == marker::MARKER_FILE_NAME)
    {
        return false;
    }
    journal
        .sqlite
        .as_ref()
        .is_none_or(|entry| sqlite::validate_journal(entry).is_ok())
}

/// 限定迁移目标到受控配置、Agent/workspace 和 vault 元数据，避免通用事务触碰任意秘密路径。
pub(super) fn is_managed_path(path: &str) -> bool {
    if path.contains('\\')
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return false;
    }
    if path == "settings.toml"
        || path == "hosts/groups.toml"
        || path == "secrets/vault.toml"
        || path == "kerminal-config.md"
        || path == "window-state.json"
        || path == "workspace/session.json"
        || path == "data/port-forwards/definitions.json"
        || path == "data/port-forwards/sessions.json"
        || path == marker::MARKER_FILE_NAME
    {
        return true;
    }
    let parts = path.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [directory, file]
            if matches!(*directory, "profiles" | "hosts" | "snippets" | "workflows") =>
        {
            file.ends_with(".toml") && *file != ".toml" && *file != "groups.toml"
        }
        ["agents", "sessions", id, file] if !id.is_empty() => {
            matches!(*file, "session.toml" | "provider.toml")
        }
        ["agents", "sessions", id, "context", file] if !id.is_empty() => matches!(
            *file,
            "mcp-endpoint.json" | "target-binding.json" | "terminal-snapshot.json"
        ),
        _ => false,
    }
}

/// 将原始 planner DTO 转为仅含路径与摘要的 journal 项。
pub(super) fn file_entry(edit: &FileEdit) -> FileJournalEntry {
    FileJournalEntry {
        relative_path: edit.relative_path.to_string_lossy().replace('\\', "/"),
        before_sha256: edit.before.as_deref().map(sha256),
        after_sha256: sha256(&edit.after),
    }
}

/// 对完整文件字节生成稳定的小写 SHA-256，用于提交和恢复时比较实际状态。
pub(super) fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 以独占方式创建 journal、staging 或备份文件，并同步其内容后才返回。
pub(super) fn write_new_file(path: &Path, bytes: &[u8]) -> AppResult<()> {
    ensure_parent_directories(path)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error("创建迁移文件失败", source))?;
    file.write_all(bytes)
        .map_err(|source| io_error("写入迁移文件失败", source))?;
    file.sync_all()
        .map_err(|source| io_error("同步迁移文件失败", source))
}

/// 逐层拒绝链接和非目录项，避免 staging/backup 路径穿过用户控制的重解析点。
pub(super) fn ensure_directory(root: &Path, directory: &Path) -> AppResult<()> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| AppError::conflict("迁移目录越过数据根"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(AppError::conflict("迁移目录路径无效"));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
            Ok(_) => return Err(AppError::conflict("迁移目录包含链接或非目录项")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|source| io_error("创建迁移目录失败", source))?;
            }
            Err(source) => return Err(io_error("检查迁移目录失败", source)),
        }
    }
    Ok(())
}

/// 创建不越过根目录的父目录链，便于测试和事务 staging 复用同一链接保护。
fn ensure_parent_directories(file: &Path) -> AppResult<()> {
    let parent = file
        .parent()
        .ok_or_else(|| AppError::invalid("迁移文件没有父目录"))?;
    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                current.push(part);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
                    Ok(_) => return Err(AppError::conflict("迁移文件父目录包含链接或非目录项")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&current)
                            .map_err(|source| io_error("创建迁移目录失败", source))?;
                    }
                    Err(source) => return Err(io_error("检查迁移目录失败", source)),
                }
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(AppError::conflict("迁移文件父目录路径无效")),
        }
    }
    Ok(())
}

/// 仅删除本次 CreateNew 产生且原子提交明确未发生的 journal staging 文件。
fn remove_owned_stage(stage: &Path) {
    match fs::symlink_metadata(stage) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => {
            let _ = fs::remove_file(stage);
        }
        _ => {}
    }
}

/// 比较完整的规范 journal 值，便于处理原子提交后目录同步失败的未知结果。
fn same_journal(left: &MigrationJournal, right: &MigrationJournal) -> bool {
    serde_json::to_vec(left).ok() == serde_json::to_vec(right).ok()
}

/// 摘要必须是 64 位小写十六进制；拒绝短摘要和路径注入载荷。
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// 创建单项阻断结果，供只读扫描保留恢复材料并明确拒绝自动继续。
fn blocked_scan(blocker: DataMigrationBlocker) -> JournalScan {
    JournalScan {
        blockers: vec![blocker],
        recovery_pending: false,
        committed: false,
        target_migration_contract: None,
        archived_operation_ids: Vec::new(),
    }
}

/// 保留底层 I/O source 供诊断，同时不在公开错误里带出用户路径。
fn io_error(context: &'static str, source: io::Error) -> AppError {
    AppError::infrastructure_with_source(context, source)
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_journal_unit.rs"]
mod tests;
