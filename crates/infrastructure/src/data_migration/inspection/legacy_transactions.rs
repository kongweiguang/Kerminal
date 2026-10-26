//! 识别旧 FileStore 的终态存档；只核对历史收据，不执行、删除或重放其中的路径。
//!
//! @author kongweiguang

use std::collections::HashSet;

use super::*;

const TRANSACTION_ROOT: &str = ".storage-transactions";

/// 未知、活动或混合恢复材料继续阻断；只有独立终态文件且总清单确认完成的存档可忽略。
pub(super) fn inspect_archives(
    root: &Path,
    blockers: &mut Vec<DataMigrationBlocker>,
) -> AppResult<()> {
    let manifest =
        read_optional_regular_file(root, Path::new("storage-manifest.toml"), MAX_DOCUMENT_BYTES)?
            .and_then(|bytes| {
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|source| toml::from_str::<OldStorageManifest>(source).ok())
            });
    for entry in
        fs::read_dir(root.join(TRANSACTION_ROOT)).map_err(io_error("读取旧事务目录失败"))?
    {
        let entry = entry.map_err(io_error("读取旧事务目录失败"))?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("profile-default.json" | "host-group.json")
        ) {
            continue;
        }
        let metadata = entry.file_type().map_err(io_error("读取旧事务目录失败"))?;
        if !metadata.is_dir() || metadata.is_symlink() {
            blockers.push(pending_archive());
            continue;
        }
        let children = fs::read_dir(entry.path())
            .map_err(io_error("读取旧事务存档失败"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_error("读取旧事务存档失败"))?;
        let Some(receipt) = children.first().filter(|_| children.len() == 1) else {
            blockers.push(pending_archive());
            continue;
        };
        let receipt_name = receipt.file_name();
        let (phase, status) = match receipt_name.to_str() {
            Some("committed.toml") => ("committed", "applied"),
            Some("rolled-back.toml") => ("rolled_back", "repaired"),
            _ => {
                blockers.push(pending_archive());
                continue;
            }
        };
        let relative = Path::new(TRANSACTION_ROOT).join(&name).join(&receipt_name);
        let journal = read_optional_regular_file(root, &relative, MAX_DOCUMENT_BYTES)
            .ok()
            .flatten()
            .and_then(|bytes| {
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|source| toml::from_str::<TerminalJournal>(source).ok())
            });
        if !journal.as_ref().is_some_and(|journal| {
            valid_archive(journal, name.to_str(), phase, status, manifest.as_ref())
        }) {
            blockers.push(DataMigrationBlocker::CorruptLegacyTransaction {
                relative_path: TRANSACTION_ROOT.to_owned(),
            });
        }
    }
    Ok(())
}

/// 统一使用固定相对目录提示，未知条目和未完成事务都保留原材料交由旧版恢复。
fn pending_archive() -> DataMigrationBlocker {
    DataMigrationBlocker::PendingLegacyTransaction {
        relative_path: TRANSACTION_ROOT.to_owned(),
    }
}

/// 历史目标可能已被后续事务修改，因此核对收据身份与清单，不拿旧 after 摘要要求当前文件回退。
fn valid_archive(
    journal: &TerminalJournal,
    directory_id: Option<&str>,
    phase: &str,
    status: &str,
    manifest: Option<&OldStorageManifest>,
) -> bool {
    let Some(manifest) = manifest.filter(|manifest| {
        manifest.schema_version == 1
            && manifest.active_change_set_id.is_none()
            && manifest.repair_state.is_none()
    }) else {
        return false;
    };
    let changes: Vec<_> = manifest
        .change_sets
        .iter()
        .filter(|change| change.id == journal.id)
        .collect();
    let [change] = changes.as_slice() else {
        return false;
    };
    if journal.schema_version != 1
        || directory_id != Some(journal.id.as_str())
        || journal.phase != phase
        || journal.started_at.trim().is_empty()
        || change.started_at != journal.started_at
        || change.status != status
        || !change
            .completed_at
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || journal.entries.is_empty()
    {
        return false;
    }
    let mut targets = HashSet::new();
    for entry in &journal.entries {
        if !valid_entry(entry, &journal.id, phase == "committed")
            || !targets.insert(entry.relative_path.as_str())
        {
            return false;
        }
    }
    let declared: HashSet<_> = change.touched_files.iter().map(String::as_str).collect();
    declared.len() == change.touched_files.len() && declared == targets
}

/// 只接受旧 FileStore 的已知记录结构和路径关系；校验不访问收据内的备份或 staged 路径。
fn valid_entry(entry: &TerminalEntry, id: &str, committed: bool) -> bool {
    let path = &entry.relative_path;
    if path.is_empty()
        || path.contains(['\\', ':'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return false;
    }
    if entry.original_exists {
        if entry.backup_path.as_deref() != Some(format!("backups/{id}/{path}").as_str())
            || (committed && entry.original_sha256.is_none())
        {
            return false;
        }
    } else if entry.backup_path.is_some() || entry.original_sha256.is_some() {
        return false;
    }
    if entry
        .original_sha256
        .as_ref()
        .is_some_and(|digest| !valid_digest(digest))
    {
        return false;
    }
    match entry.action.as_str() {
        "write" => {
            entry.staged_path.as_deref()
                == Some(format!("{TRANSACTION_ROOT}/{id}/staged/{path}").as_str())
                && entry
                    .intended_sha256
                    .as_ref()
                    .is_some_and(|digest| valid_digest(digest))
        }
        "delete" => entry.staged_path.is_none() && entry.intended_sha256.is_none(),
        _ => false,
    }
}

/// 固定旧版 sha256 前缀与摘要长度，避免任意缺失内容被伪装成完整的已提交收据。
fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalJournal {
    schema_version: u32,
    id: String,
    started_at: String,
    phase: String,
    #[serde(rename = "last_error")]
    _last_error: Option<String>,
    entries: Vec<TerminalEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalEntry {
    relative_path: String,
    original_exists: bool,
    backup_path: Option<String>,
    original_sha256: Option<String>,
    action: String,
    staged_path: Option<String>,
    intended_sha256: Option<String>,
}
