//! 迁移状态的只读扫描与业务 planner 汇总。
//!
//! @author kongweiguang

use std::{
    fs,
    fs::File,
    io::{self, Read},
    path::{Component, Path},
};

use kerminal_application::{
    AppError, AppResult,
    data_migration::{
        DataMigrationBlocker, DataMigrationInspection, DataMigrationNotice, DataMigrationReadiness,
    },
};
use serde::Deserialize;
use serde_json::Value;

use super::{
    FileEdit, agents, configuration, marker, operational_state, transaction, vault, window_state,
    workspace,
};

mod legacy_transactions;

const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STORAGE_LOCK_BYTES: u64 = 4 * 1024;
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
const CURRENT_SQLITE_SCHEMA: u32 = 3;

/// 已知上一契约走显式 planner，当前契约校验新增持久状态；所有检查保持无写入与无数据库连接。
pub(super) fn inspect_root(
    root: &Path,
    legacy_window_state: Option<&Path>,
) -> AppResult<DataMigrationInspection> {
    if !path_exists_without_following(root)? {
        return Ok(inspection(
            DataMigrationReadiness::Fresh,
            None,
            None,
            vec![],
            vec![],
            false,
        ));
    }
    let root_meta = fs::symlink_metadata(root).map_err(io_error("检查用户数据根失败"))?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Ok(blocked(DataMigrationBlocker::UnknownNonEmptyRoot));
    }

    let marker_read = marker::read_marker(root)?;
    let journal_scan = transaction::inspect_journals(root)?;
    let mut blockers = journal_scan.blockers.clone();

    match marker_read {
        marker::MarkerRead::Blocked(blocker) => blockers.push(blocker),
        marker::MarkerRead::Current => {
            if journal_scan.target_migration_contract == Some(marker::PREVIOUS_MIGRATION_CONTRACT) {
                blockers.push(DataMigrationBlocker::CorruptMigrationJournal);
            }
            if let Err(blocker) = operational_state::validate_current_operational_state(root) {
                blockers.push(blocker);
            }
            return Ok(inspection(
                if blockers.is_empty() {
                    DataMigrationReadiness::Current
                } else {
                    DataMigrationReadiness::Blocked
                },
                Some(marker::CURRENT_DATA_FORMAT),
                Some(marker::CURRENT_MIGRATION_CONTRACT),
                blockers,
                vec![],
                journal_scan.recovery_pending,
            ));
        }
        marker::MarkerRead::PreviousSupported => {
            if journal_scan.committed
                && journal_scan.target_migration_contract
                    == Some(marker::CURRENT_MIGRATION_CONTRACT)
            {
                blockers.push(DataMigrationBlocker::CorruptMigrationJournal);
            }
            if !journal_scan.recovery_pending && blockers.is_empty() {
                match collect_plan(root, legacy_window_state) {
                    Ok(edits) => {
                        if let Err(blocker) = validate_edits(root, &edits) {
                            blockers.push(blocker);
                        }
                    }
                    Err(blocker) => blockers.push(blocker),
                }
            }
            return Ok(inspection(
                if blockers.is_empty() {
                    DataMigrationReadiness::Legacy
                } else {
                    DataMigrationReadiness::Blocked
                },
                Some(marker::CURRENT_DATA_FORMAT),
                Some(marker::PREVIOUS_MIGRATION_CONTRACT),
                blockers,
                vec![],
                journal_scan.recovery_pending,
            ));
        }
        marker::MarkerRead::Missing => {}
    }

    if journal_scan.committed {
        blockers.push(DataMigrationBlocker::CorruptMigrationJournal);
    }
    if journal_scan.recovery_pending && blockers.is_empty() {
        // 已发布 journal 表示业务文件可能已部分前滚；恢复必须沿用原计划和 CAS，不能重算。
        return Ok(inspection(
            DataMigrationReadiness::Legacy,
            None,
            None,
            vec![],
            vec![],
            true,
        ));
    }

    inspect_old_storage_transactions(root, &mut blockers)?;
    inspect_old_manifest(root, &mut blockers)?;
    inspect_workspace(root, &mut blockers)?;

    if !blockers.is_empty() {
        return Ok(inspection(
            DataMigrationReadiness::Blocked,
            None,
            None,
            blockers,
            vec![],
            journal_scan.recovery_pending,
        ));
    }

    let entries = read_root_entries(root)?;
    if entries.is_empty() {
        return Ok(inspection(
            DataMigrationReadiness::Fresh,
            None,
            None,
            vec![],
            vec![],
            false,
        ));
    }
    if !has_kerminal_signature(root, &entries)? {
        return Ok(blocked(DataMigrationBlocker::UnknownNonEmptyRoot));
    }

    let mut notices = Vec::new();
    inspect_legacy_sqlite(root, &mut blockers, &mut notices)?;
    let mut edits = match collect_plan(root, legacy_window_state) {
        Ok(edits) => edits,
        Err(blocker) => {
            blockers.push(blocker);
            vec![]
        }
    };
    if let Err(blocker) = validate_edits(root, &edits) {
        blockers.push(blocker);
    }
    edits.clear();

    let readiness = if blockers.is_empty() {
        DataMigrationReadiness::Legacy
    } else {
        DataMigrationReadiness::Blocked
    };
    Ok(inspection(
        readiness,
        None,
        None,
        blockers,
        notices,
        journal_scan.recovery_pending,
    ))
}

/// 当前 marker 快速路径仍验证新增恢复文档；上一契约绝不通过该路径启动普通 runtime。
pub(super) fn inspect_current_fast_path(root: &Path) -> AppResult<Option<DataMigrationInspection>> {
    if !path_exists_without_following(root)? {
        return Ok(None);
    }
    let root_metadata = fs::symlink_metadata(root).map_err(io_error("检查用户数据根失败"))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Ok(Some(blocked(DataMigrationBlocker::UnknownNonEmptyRoot)));
    }
    match marker::read_marker(root)? {
        marker::MarkerRead::Current => {
            let mut journal_scan = transaction::inspect_journals(root)?;
            if journal_scan.target_migration_contract == Some(marker::PREVIOUS_MIGRATION_CONTRACT) {
                journal_scan
                    .blockers
                    .push(DataMigrationBlocker::CorruptMigrationJournal);
            }
            if let Err(blocker) = operational_state::validate_current_operational_state(root) {
                journal_scan.blockers.push(blocker);
            }
            let readiness = if journal_scan.blockers.is_empty() {
                DataMigrationReadiness::Current
            } else {
                DataMigrationReadiness::Blocked
            };
            Ok(Some(inspection(
                readiness,
                Some(marker::CURRENT_DATA_FORMAT),
                Some(marker::CURRENT_MIGRATION_CONTRACT),
                journal_scan.blockers,
                vec![],
                journal_scan.recovery_pending,
            )))
        }
        marker::MarkerRead::Blocked(blocker) => Ok(Some(blocked(blocker))),
        marker::MarkerRead::Missing | marker::MarkerRead::PreviousSupported => Ok(None),
    }
}

/// 合并各业务 planner 的变更；错误只映射为安全分类，不把路径或配置正文写入 DTO。
pub(super) fn collect_plan(
    root: &Path,
    legacy_window_state: Option<&Path>,
) -> Result<Vec<FileEdit>, DataMigrationBlocker> {
    let mut edits = Vec::new();
    edits.extend(
        configuration::plan_configuration_migration(root)
            .map_err(|_| DataMigrationBlocker::ConfigurationNotMigratable)?,
    );
    edits.extend(
        agents::plan_agent_migration(root)
            .map_err(|_| DataMigrationBlocker::AgentWorkspaceNotMigratable)?,
    );
    let workspace_source = read_optional_regular_file(
        root,
        Path::new("workspace/session.json"),
        MAX_DOCUMENT_BYTES,
    )
    .map_err(|_| DataMigrationBlocker::ManagedDataInvalid {
        relative_path: "workspace/session.json".to_owned(),
    })?;
    if let Some(edit) = workspace::plan_workspace_migration(workspace_source).map_err(|_| {
        DataMigrationBlocker::ManagedDataInvalid {
            relative_path: "workspace/session.json".to_owned(),
        }
    })? {
        edits.push(edit);
    }
    edits.extend(vault::plan_vault_migration(root).map_err(|_| {
        DataMigrationBlocker::ManagedDataInvalid {
            relative_path: "secrets/vault.toml".to_owned(),
        }
    })?);
    edits.extend(
        window_state::plan_window_state_migration(root, legacy_window_state)
            .map_err(|_| DataMigrationBlocker::WindowStateNotMigratable)?,
    );
    operational_state::validate_current_operational_state(root)?;
    edits.extend(
        operational_state::plan_operational_state_migration(root).map_err(|_| {
            DataMigrationBlocker::ManagedDataInvalid {
                relative_path: operational_state::LEGACY_FORWARDS.to_owned(),
            }
        })?,
    );
    Ok(edits)
}

/// 仅允许受控配置与稳定运行定义；旧转发摘要作为原文 CAS/备份项，不能被修改。
pub(super) fn validate_edits(root: &Path, edits: &[FileEdit]) -> Result<(), DataMigrationBlocker> {
    let mut seen = std::collections::BTreeSet::new();
    for edit in edits {
        let path = &edit.relative_path;
        let normalized = path.to_string_lossy().replace('\\', "/");
        let safe_components = path
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
        let agent_path = Path::new(&normalized);
        let agent_parts = agent_path.components().collect::<Vec<_>>();
        let agent_shape = match agent_parts.as_slice() {
            [
                Component::Normal(agents),
                Component::Normal(sessions),
                Component::Normal(id),
                Component::Normal(file),
            ] if agents.to_str() == Some("agents")
                && sessions.to_str() == Some("sessions")
                && !id.is_empty() =>
            {
                matches!(file.to_str(), Some("session.toml" | "provider.toml"))
            }
            [
                Component::Normal(agents),
                Component::Normal(sessions),
                Component::Normal(id),
                Component::Normal(context),
                Component::Normal(file),
            ] if agents.to_str() == Some("agents")
                && sessions.to_str() == Some("sessions")
                && context.to_str() == Some("context")
                && !id.is_empty() =>
            {
                matches!(
                    file.to_str(),
                    Some("mcp-endpoint.json" | "target-binding.json" | "terminal-snapshot.json")
                )
            }
            _ => false,
        };
        let config_path = match agent_parts.as_slice() {
            [Component::Normal(directory), Component::Normal(file)]
                if matches!(
                    directory.to_str(),
                    Some("profiles" | "hosts" | "snippets" | "workflows")
                ) =>
            {
                file.to_str().is_some_and(|name| {
                    name.ends_with(".toml") && name != ".toml" && name != "groups.toml"
                })
            }
            _ => false,
        };
        let allowed = config_path
            || normalized == "settings.toml"
            || normalized == "hosts/groups.toml"
            || normalized == "secrets/vault.toml"
            || normalized == "kerminal-config.md"
            || normalized == marker::MARKER_FILE_NAME
            || agent_shape
            || normalized == "workspace/session.json"
            || normalized == "window-state.json";
        let allowed = allowed
            || normalized == "data/port-forwards/definitions.json"
            || (normalized == operational_state::LEGACY_FORWARDS
                && edit.before.as_deref() == Some(edit.after.as_slice()));
        if !safe_components
            || !allowed
            || !seen.insert(normalized.clone())
            || !edit_path_is_safe(root, path, edit.before.as_deref())
        {
            return Err(DataMigrationBlocker::ManagedDataInvalid {
                relative_path: safe_relative_hint(&normalized),
            });
        }
    }
    Ok(())
}

/// 旧 marker 缺失时只用已知受管文件辨认 Kerminal 根，不依赖 app semver 或猜测来源版本。
fn has_kerminal_signature(root: &Path, entries: &[String]) -> AppResult<bool> {
    for name in [
        "settings.toml",
        "storage-manifest.toml",
        ".storage-transactions",
    ] {
        if entries.iter().any(|entry| entry == name) {
            return Ok(true);
        }
    }
    for (directory, suffix) in [
        ("profiles", ".toml"),
        ("hosts", ".toml"),
        ("snippets", ".toml"),
        ("workflows", ".toml"),
    ] {
        let path = root.join(directory);
        if directory_has_matching_file(&path, suffix)? {
            return Ok(true);
        }
    }
    for name in [
        "workspace/session.json",
        "data/command.sqlite",
        "secrets/vault.toml",
        "secrets/vault-key.toml",
        operational_state::LEGACY_FORWARDS,
        "data/port-forwards/definitions.json",
        "data/transfer-recovery.json",
    ] {
        if path_exists_without_following(&root.join(name))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// SQLite dry-run 仅检查主库 magic、user_version 与 WAL 是否存在，避免打开连接生成 -shm。
fn inspect_legacy_sqlite(
    root: &Path,
    blockers: &mut Vec<DataMigrationBlocker>,
    notices: &mut Vec<DataMigrationNotice>,
) -> AppResult<()> {
    let database = root.join("data/command.sqlite");
    let wal = root.join("data/command.sqlite-wal");
    let shm = root.join("data/command.sqlite-shm");
    let has_database = path_exists_without_following(&database)?;
    let has_wal = path_exists_without_following(&wal)?;
    let has_shm = path_exists_without_following(&shm)?;
    if !has_database {
        if has_wal || has_shm {
            blockers.push(DataMigrationBlocker::CorruptCommandHistoryDatabase);
        }
        return Ok(());
    }
    let bytes = read_file_prefix(root, Path::new("data/command.sqlite"), 100)?;
    if bytes.len() < 100 || !bytes.starts_with(SQLITE_HEADER) {
        blockers.push(DataMigrationBlocker::CorruptCommandHistoryDatabase);
        return Ok(());
    }
    let schema_version = u32::from_be_bytes([bytes[60], bytes[61], bytes[62], bytes[63]]);
    if has_wal {
        notices.push(DataMigrationNotice::SqliteWalPresent);
    } else if has_shm {
        blockers.push(DataMigrationBlocker::CorruptCommandHistoryDatabase);
    } else if schema_version > CURRENT_SQLITE_SCHEMA {
        blockers.push(DataMigrationBlocker::UnsupportedCommandHistorySchema { schema_version });
    }
    Ok(())
}

/// `storage-manifest.toml` 是旧恢复证据；按 serde DTO 规则接受缺失 Option 字段并阻断未完成状态。
fn inspect_old_manifest(root: &Path, blockers: &mut Vec<DataMigrationBlocker>) -> AppResult<()> {
    let relative = Path::new("storage-manifest.toml");
    let Some(bytes) = read_optional_regular_file(root, relative, MAX_DOCUMENT_BYTES)? else {
        return Ok(());
    };
    let parsed = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|source| toml::from_str::<OldStorageManifest>(source).ok());
    let Some(manifest) = parsed else {
        blockers.push(DataMigrationBlocker::CorruptLegacyTransaction {
            relative_path: "storage-manifest.toml".to_owned(),
        });
        return Ok(());
    };
    let malformed = manifest.change_sets.iter().any(|change| {
        change.id.trim().is_empty()
            || change.started_at.trim().is_empty()
            || !matches!(
                change.status.as_str(),
                "started" | "applied" | "failed" | "repaired"
            )
    });
    if malformed {
        blockers.push(DataMigrationBlocker::CorruptLegacyTransaction {
            relative_path: "storage-manifest.toml".to_owned(),
        });
        return Ok(());
    }
    if manifest.schema_version != 1
        || manifest.active_change_set_id.is_some()
        || manifest.repair_state.is_some()
        || manifest
            .change_sets
            .iter()
            .any(|change| matches!(change.status.as_str(), "started" | "failed"))
    {
        blockers.push(DataMigrationBlocker::PendingLegacyTransaction {
            relative_path: "storage-manifest.toml".to_owned(),
        });
    }
    Ok(())
}

/// 活动旧 journal 仍交给旧恢复流程；已完成的目录式事务仅在收据与 manifest 一致时放行。
///
/// @author kongweiguang
fn inspect_old_storage_transactions(
    root: &Path,
    blockers: &mut Vec<DataMigrationBlocker>,
) -> AppResult<()> {
    for relative in [
        ".storage-transactions/profile-default.json",
        ".storage-transactions/host-group.json",
    ] {
        let Some(bytes) =
            read_optional_regular_file(root, Path::new(relative), MAX_DOCUMENT_BYTES)?
        else {
            continue;
        };
        let valid = if relative.ends_with("profile-default.json") {
            serde_json::from_slice::<OldProfileJournal>(&bytes).is_ok_and(|journal| {
                journal.schema_version == 1
                    && !journal.entries.is_empty()
                    && journal.entries.iter().all(valid_old_journal_entry)
            })
        } else {
            serde_json::from_slice::<OldHostGroupJournal>(&bytes).is_ok_and(|journal| {
                journal.schema_version == 1
                    && !journal.operation.deleted_group_id.is_empty()
                    && !journal.entries.is_empty()
                    && journal
                        .operation
                        .destination_group_id
                        .as_deref()
                        .is_none_or(|id| !id.is_empty())
                    && journal.entries.iter().all(valid_old_journal_entry)
            })
        };
        blockers.push(if valid {
            DataMigrationBlocker::PendingLegacyTransaction {
                relative_path: relative.to_owned(),
            }
        } else {
            DataMigrationBlocker::CorruptLegacyTransaction {
                relative_path: relative.to_owned(),
            }
        });
    }
    let transactions = root.join(".storage-transactions");
    if path_exists_without_following(&transactions)? {
        let metadata =
            fs::symlink_metadata(&transactions).map_err(io_error("读取旧事务目录失败"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            blockers.push(DataMigrationBlocker::CorruptLegacyTransaction {
                relative_path: ".storage-transactions".to_owned(),
            });
        } else {
            legacy_transactions::inspect_archives(root, blockers)?;
        }
    }
    Ok(())
}

/// 无 marker 时只接受当前有效 v3 或可明确补齐的已知缺省；旧版本和未知字段值保持阻断。
fn inspect_workspace(root: &Path, blockers: &mut Vec<DataMigrationBlocker>) -> AppResult<()> {
    let relative = Path::new("workspace/session.json");
    let Some(bytes) = read_optional_regular_file(root, relative, MAX_DOCUMENT_BYTES)? else {
        return Ok(());
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            blockers.push(DataMigrationBlocker::ManagedDataInvalid {
                relative_path: "workspace/session.json".to_owned(),
            });
            return Ok(());
        }
    };
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    if version != Some(3) {
        blockers.push(DataMigrationBlocker::UnsupportedWorkspaceSessionSchema { version });
    } else if workspace::normalize_workspace_bytes(&bytes).is_err() {
        blockers.push(DataMigrationBlocker::ManagedDataInvalid {
            relative_path: "workspace/session.json".to_owned(),
        });
    }
    Ok(())
}

/// 根目录入口只列出一层名称用于识别；不递归读取 transcript、vendor 或用户自定义目录。
fn read_root_entries(root: &Path) -> AppResult<Vec<String>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(root).map_err(io_error("读取用户数据根目录失败"))? {
        let item = item.map_err(io_error("读取用户数据根目录失败"))?;
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".storage.lock" && is_valid_storage_lock(root)? {
            continue;
        }
        if name == ".kerminal-migration" && is_empty_migration_directory(root)? {
            continue;
        }
        entries.push(name);
    }
    Ok(entries)
}

/// 只把符合当前锁 schema 的普通锁文件视为控制元数据，避免任意同名用户文件被忽略。
fn is_valid_storage_lock(root: &Path) -> AppResult<bool> {
    let Some(bytes) =
        read_optional_regular_file(root, Path::new(".storage.lock"), MAX_STORAGE_LOCK_BYTES)?
    else {
        return Ok(false);
    };
    let parsed = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|source| toml::from_str::<StorageLockDocument>(source).ok());
    Ok(parsed.is_some_and(|lock| {
        lock.schema_version == 1
            && lock.pid > 0
            && lock.process_started_at_unix_seconds > 0
            && lock.created_at_unix_ms > 0
            && uuid::Uuid::parse_str(&lock.nonce).is_ok()
    }))
}

/// 空的迁移控制目录可留在Fresh根中重试；存在任何内容时交由journal scanner阻断。
fn is_empty_migration_directory(root: &Path) -> AppResult<bool> {
    let path = root.join(".kerminal-migration");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(io_error("检查迁移控制目录失败")(source)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let mut entries = fs::read_dir(path).map_err(io_error("读取迁移控制目录失败"))?;
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(source)) => Err(io_error("读取迁移控制目录失败")(source)),
    }
}

/// 在目标目录中只检查普通一层 TOML 文件，便于既有配置类别成为 legacy 根签名。
fn directory_has_matching_file(directory: &Path, suffix: &str) -> AppResult<bool> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取配置目录失败",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(directory).map_err(io_error("读取配置目录失败"))? {
        let entry = entry.map_err(io_error("读取配置目录失败"))?;
        let item_meta = entry.metadata().map_err(io_error("读取配置目录失败"))?;
        if item_meta.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// SQLite 只取固定头部字节，避免读完整历史数据库或打开连接生成 WAL/SHM。
fn read_file_prefix(root: &Path, relative: &Path, count: usize) -> AppResult<Vec<u8>> {
    let absolute = root.join(relative);
    let metadata = fs::symlink_metadata(&absolute).map_err(io_error("读取命令历史头部失败"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::conflict("命令历史路径不是普通文件"));
    }
    let mut file = File::open(absolute).map_err(io_error("读取命令历史头部失败"))?;
    let mut bytes = vec![0; count];
    let mut read = 0;
    while read < count {
        let amount = file
            .read(&mut bytes[read..])
            .map_err(io_error("读取命令历史头部失败"))?;
        if amount == 0 {
            break;
        }
        read += amount;
    }
    bytes.truncate(read);
    Ok(bytes)
}

/// 只读取普通 managed 文件；`None` 表示末端缺失，链接、目录和超大文件均拒绝。
pub(super) fn read_optional_regular_file(
    root: &Path,
    relative: &Path,
    max_bytes: u64,
) -> AppResult<Option<Vec<u8>>> {
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(AppError::invalid("迁移预检路径无效"));
    }
    let mut current = root.to_path_buf();
    let parts = relative.components().collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        current.push(part.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "读取迁移预检文件失败",
                    source,
                ));
            }
        };
        let is_last = index + 1 == parts.len();
        if metadata.file_type().is_symlink()
            || if is_last {
                !metadata.is_file() || metadata.len() > max_bytes
            } else {
                !metadata.is_dir()
            }
        {
            return Err(AppError::conflict("迁移预检路径包含链接或非普通文件"));
        }
    }
    fs::read(current)
        .map(Some)
        .map_err(|source| AppError::infrastructure_with_source("读取迁移预检文件失败", source))
}

/// 对末端作不跟随链接的存在性检查，供只读状态判定调用。
fn path_exists_without_following(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(AppError::infrastructure_with_source(
            "检查迁移路径失败",
            source,
        )),
    }
}

/// 结构化旧 manifest 与两个旧 journal DTO；缺失 Option 字段按 serde 默认语义接受。
#[derive(Deserialize)]
struct OldStorageManifest {
    schema_version: u32,
    active_change_set_id: Option<String>,
    repair_state: Option<serde_json::Value>,
    change_sets: Vec<OldManifestChangeSet>,
}

#[derive(Deserialize)]
struct OldManifestChangeSet {
    id: String,
    status: String,
    started_at: String,
    completed_at: Option<String>,
    #[serde(default)]
    touched_files: Vec<String>,
}

/// 与存储协调器当前 schema v1 对齐，只用于根目录枚举时识别其保留锁文件。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageLockDocument {
    schema_version: u32,
    pid: u32,
    process_started_at_unix_seconds: u64,
    created_at_unix_ms: u64,
    nonce: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OldProfileJournal {
    schema_version: u32,
    entries: Vec<OldJournalEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OldHostGroupJournal {
    schema_version: u32,
    operation: OldHostGroupOperation,
    entries: Vec<OldJournalEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OldHostGroupOperation {
    deleted_group_id: String,
    destination_group_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OldJournalEntry {
    relative_path: String,
    before_revision: String,
    after_source: String,
}

/// 统一构造没有敏感绝对路径的 inspection DTO。
fn inspection(
    readiness: DataMigrationReadiness,
    data_format: Option<u32>,
    migration_contract: Option<u32>,
    blockers: Vec<DataMigrationBlocker>,
    notices: Vec<DataMigrationNotice>,
    recovery_pending: bool,
) -> DataMigrationInspection {
    DataMigrationInspection {
        readiness,
        data_format,
        migration_contract,
        blockers,
        notices,
        recovery_pending,
    }
}

/// 构造单个阻断项 DTO，调用方不需要复制序列化字段规则。
fn blocked(blocker: DataMigrationBlocker) -> DataMigrationInspection {
    inspection(
        DataMigrationReadiness::Blocked,
        None,
        None,
        vec![blocker],
        vec![],
        false,
    )
}

/// 仅允许相对路径错误 hint，防止将根目录或用户文件名写进安全报告。
fn safe_relative_hint(_path: &str) -> String {
    "managed-path".to_owned()
}

/// 将系统 I/O 原因保留为内部 source，同时保证公开迁移错误不回显绝对路径。
fn io_error(context: &'static str) -> impl FnOnce(io::Error) -> AppError {
    move |source| AppError::infrastructure_with_source(context, source)
}

/// 旧 journal 条目必须包含可核对的原 revision 和完整目标内容。
fn valid_old_journal_entry(entry: &OldJournalEntry) -> bool {
    !entry.relative_path.trim().is_empty()
        && entry.before_revision.len() == 64
        && entry
            .before_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && !entry.after_source.is_empty()
}

/// planner 输入在提交前会另做 CAS；预检阶段先拒绝链接并核对 planner 的 before 原文。
fn edit_path_is_safe(root: &Path, relative: &Path, before: Option<&[u8]>) -> bool {
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return false;
    }
    let mut current = root.to_path_buf();
    let parts = relative.components().collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || if index + 1 == parts.len() {
                        !metadata.is_file()
                    } else {
                        !metadata.is_dir()
                    }
                {
                    return false;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && index + 1 == parts.len() => {
                return before.is_none();
            }
            Err(_) => return false,
        }
    }
    match (before, fs::read(current)) {
        (Some(expected), Ok(actual)) => expected == actual,
        (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => true,
        _ => false,
    }
}
