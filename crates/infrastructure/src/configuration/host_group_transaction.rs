//! 主机删除与分组迁移事务；固定 journal 只覆盖已验证的 hosts 文档。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use kerminal_application::connections::management::HostDocumentRevision;
use kerminal_application::{AppError, AppResult, PublicErrorCode};
use kerminal_domain::preferences::{ConfigDocument, ConfigDocumentPath, ConfigRevision};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Value};

use crate::storage::{
    AtomicCommitError, CommitMode, StorageCoordinator, remove_file_with_parent_sync,
};

use super::{
    ConfigurationStore, atomic_replace, final_path_is_symlink, read_configuration_file,
    relative_path, revision_for, safe_join, unix_millis_string, unreadable_issue,
};

const HOST_GROUP_JOURNAL: &str = ".storage-transactions/host-group.json";
const MAX_HOST_GROUP_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const GROUP_SCHEMA_VERSION: u32 = 1;

/// 分组事务的已确认结果；完整 groups 文档与 revision 均来自同一原子事务。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostGroupTransactionResult {
    pub(crate) groups: ConfigDocument,
    pub(crate) reassigned_hosts: usize,
}

#[derive(Clone, Debug)]
struct HostDocumentSnapshot {
    source: String,
    revision: ConfigRevision,
    group_id: Option<String>,
}

impl ConfigurationStore {
    /// 以共享锁和原始字节 CAS 删除单个 host；删除后目录同步未知时不触发凭据清理。
    pub(crate) fn delete_host_document(
        &self,
        host_id: &kerminal_domain::identity::HostId,
        expected_revision: &ConfigRevision,
    ) -> AppResult<bool> {
        self.ensure_writable()?;
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_writable()?;
        self.ensure_configuration_transactions_absent()?;

        if !hosts_directory_exists(&self.inner.root)? {
            return Ok(false);
        }

        let document_path = ConfigDocumentPath::Host(host_id.as_str().to_owned());
        let relative = relative_path(&document_path)?;
        let absolute = safe_join(&self.inner.root, &relative)?;
        if final_path_is_symlink(&absolute).map_err(|source| {
            AppError::infrastructure_with_source("检查主机配置目标失败", source)
        })? {
            return Err(AppError::conflict("符号链接主机配置不能删除"));
        }
        let Some(source) = read_configuration_file(&absolute)
            .map_err(|source| AppError::infrastructure_with_source("读取主机配置失败", source))?
        else {
            return Ok(false);
        };
        super::validation::validate_document(&document_path, &source)
            .map_err(|source| AppError::configuration_corrupt("主机配置").with_source(source))?;
        if expected_revision != &revision_for(&source) {
            return Err(AppError::conflict(
                "主机配置已被其它操作修改，请重新读取后删除",
            ));
        }

        match remove_file_with_parent_sync(&absolute) {
            Ok(()) => Ok(true),
            Err(AtomicCommitError::NotCommitted(source))
                if source.kind() == io::ErrorKind::NotFound =>
            {
                Err(AppError::conflict("主机配置在删除期间已变化，请重新读取"))
            }
            Err(AtomicCommitError::NotCommitted(source)) => Err(
                AppError::infrastructure_with_source("删除主机配置失败", source),
            ),
            Err(AtomicCommitError::CommitUnknown(source)) => Err(
                AppError::outcome_unknown_with_source("主机删除结果未知，请重新读取后确认", source),
            ),
        }
    }

    /// 锁内复核引用再删除分组；现有 journal 仅迁移远程主机，本地主机须先显式调整归属。
    /// 不把 Profile 悄悄纳入未确认的跨文件写入，也不允许留下校验失败的悬空引用。
    pub(crate) fn delete_host_group_and_reassign(
        &self,
        group_id: String,
        destination_group_id: Option<String>,
        expected_group_revision: ConfigRevision,
        expected_hosts: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupTransactionResult> {
        self.ensure_writable()?;
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_writable()?;
        self.ensure_configuration_transactions_absent()?;

        if !is_safe_group_id(&group_id)
            || destination_group_id
                .as_deref()
                .is_some_and(|destination| !is_safe_group_id(destination))
        {
            return Err(AppError::invalid("主机分组标识无效"));
        }
        if destination_group_id.as_deref() == Some(group_id.as_str()) {
            return Err(AppError::invalid("目标分组不能是待删除分组"));
        }

        let groups_document = read_groups_document(&self.inner.root)?
            .ok_or_else(|| AppError::not_found("主机分组不存在"))?;
        if groups_document.revision != expected_group_revision {
            return Err(AppError::conflict("主机分组已变化，请刷新后重试"));
        }
        let group_ids = parse_group_ids(&groups_document.source)?;
        if !group_ids.contains(&group_id) {
            return Err(AppError::not_found("主机分组不存在"));
        }
        if destination_group_id
            .as_ref()
            .is_some_and(|destination| !group_ids.contains(destination))
        {
            return Err(AppError::not_found("目标主机分组不存在"));
        }

        if self
            .list_profiles_document_unlocked()?
            .iter()
            .any(|profile| profile.sidebar_group_id.as_deref() == Some(group_id.as_str()))
        {
            return Err(AppError::conflict(
                "分组仍有本地主机，请先调整本地主机分组后再删除",
            ));
        }

        let hosts = read_host_documents(&self.inner.root, &group_ids)?;
        validate_complete_host_revision_set(&hosts, &expected_hosts)?;
        let mut entries = Vec::new();
        let mut reassigned_hosts = 0usize;
        for (host_id, host) in &hosts {
            if host.group_id.as_deref() != Some(group_id.as_str()) {
                continue;
            }
            let path = ConfigDocumentPath::Host(host_id.clone());
            let relative = relative_path(&path)?;
            let after_source = rewrite_host_group(&host.source, destination_group_id.as_deref())?;
            super::validation::validate_document(&path, &after_source).map_err(|source| {
                AppError::configuration_corrupt("主机配置").with_source(source)
            })?;
            entries.push(HostGroupJournalEntry {
                relative_path: portable_relative(&relative),
                before_revision: host.revision.as_str().to_owned(),
                after_source,
            });
            reassigned_hosts += 1;
        }

        let after_groups = remove_group_from_source(&groups_document.source, &group_id)?;
        super::validation::validate_document(&ConfigDocumentPath::HostGroups, &after_groups)
            .map_err(|source| AppError::configuration_corrupt("主机分组").with_source(source))?;
        entries.push(HostGroupJournalEntry {
            relative_path: portable_relative(&relative_path(&ConfigDocumentPath::HostGroups)?),
            before_revision: groups_document.revision.as_str().to_owned(),
            after_source: after_groups.clone(),
        });

        let journal = HostGroupJournal {
            schema_version: 1,
            operation: HostGroupOperation {
                deleted_group_id: group_id,
                destination_group_id,
            },
            entries,
        };
        if let Err(error) = write_host_group_journal(&self.inner.root, &journal) {
            if error.public_code() == PublicErrorCode::OutcomeUnknown {
                mark_recovery_required(self, HOST_GROUP_JOURNAL);
            }
            return Err(error);
        }

        match apply_host_group_journal_locked(&self.inner.root) {
            Ok(()) => {}
            Err(original) if original.public_code() == PublicErrorCode::OutcomeUnknown => {
                mark_recovery_required(self, HOST_GROUP_JOURNAL);
                return Err(original);
            }
            Err(original) => {
                if let Err(recovery) = apply_host_group_journal_locked(&self.inner.root) {
                    mark_recovery_required(self, HOST_GROUP_JOURNAL);
                    return Err(AppError::outcome_unknown_with_source(
                        "主机分组事务恢复结果未知，请保留恢复文件并重新读取",
                        HostGroupRecoveryFailure { original, recovery },
                    ));
                }
            }
        }

        // 数据已确认全部到达目标 revision；journal 清理失败时回执仍成功，但本进程进入只读恢复态。
        if remove_host_group_journal(&self.inner.root).is_err() {
            mark_recovery_required(self, HOST_GROUP_JOURNAL);
        }
        Ok(HostGroupTransactionResult {
            groups: ConfigDocument {
                path: ConfigDocumentPath::HostGroups,
                revision: revision_for(&after_groups),
                source: after_groups,
            },
            reassigned_hosts,
        })
    }
}

/// 启动时在共享存储锁内完成 host/group journal 重放，失败保持原 journal 供可见恢复。
pub(super) fn recover_host_group_transaction(
    root: &Path,
    storage: &StorageCoordinator,
) -> AppResult<()> {
    let _lock = storage
        .lock()
        .map_err(|source| AppError::infrastructure_with_source("无法锁定配置存储", source))?;
    if read_host_group_journal(root)?.is_none() {
        return Ok(());
    }
    apply_host_group_journal_locked(root)?;
    remove_host_group_journal(root)
}

/// 普通配置读写在未完成分组 journal 存在时 fail closed，避免观察到部分迁移状态。
pub(super) fn ensure_host_group_transaction_absent(root: &Path) -> AppResult<()> {
    let transaction_directory = root.join(".storage-transactions");
    match fs::symlink_metadata(&transaction_directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::configuration_corrupt("主机分组事务目录")
                .with_source(HostGroupJournalInvalid));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "检查主机分组事务目录失败",
                source,
            ));
        }
    }
    match fs::symlink_metadata(root.join(HOST_GROUP_JOURNAL)) {
        Ok(_) => Err(AppError::configuration_corrupt("主机分组事务需要恢复")
            .with_source(HostGroupTransactionPending)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AppError::infrastructure_with_source(
            "检查主机分组事务失败",
            source,
        )),
    }
}

/// 只要 journal 清理尚未被证明完成，就把原因和相对路径保留在进程恢复状态。
fn mark_recovery_required(store: &ConfigurationStore, relative_path: &str) {
    let mut recovery = match store.inner.recovery.write() {
        Ok(recovery) => recovery,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !recovery
        .issues
        .iter()
        .any(|issue| issue.relative_path == relative_path)
    {
        recovery
            .issues
            .push(unreadable_issue(relative_path.to_owned()));
        recovery
            .issues
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    }
    recovery.mode = kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery;
}

/// 读取固定 groups 路径并拒绝链接，事务不得通过用户可控链接更新其它文件。
fn read_groups_document(root: &Path) -> AppResult<Option<ConfigDocument>> {
    if !hosts_directory_exists(root)? {
        return Ok(None);
    }
    let path = ConfigDocumentPath::HostGroups;
    let relative = relative_path(&path)?;
    let absolute = safe_join(root, &relative)?;
    if super::final_path_is_symlink(&absolute)
        .map_err(|source| AppError::infrastructure_with_source("检查主机分组失败", source))?
    {
        return Err(
            AppError::configuration_corrupt("主机分组").with_source(HostGroupDocumentInvalid)
        );
    }
    let Some(source) = read_configuration_file(&absolute)
        .map_err(|source| AppError::infrastructure_with_source("读取主机分组失败", source))?
    else {
        return Ok(None);
    };
    super::validation::validate_document(&path, &source)
        .map_err(|source| AppError::configuration_corrupt("主机分组").with_source(source))?;
    Ok(Some(ConfigDocument {
        path,
        revision: revision_for(&source),
        source,
    }))
}

/// 解码 groups schema 并验证 ID 唯一、安全，未知字段仍保留在原始文档。
fn parse_group_ids(source: &str) -> AppResult<BTreeSet<String>> {
    super::validation::validate_document(&ConfigDocumentPath::HostGroups, source)
        .map_err(|source| AppError::configuration_corrupt("主机分组").with_source(source))?;
    let document: GroupsFile = toml::from_str(source)
        .map_err(|source| AppError::configuration_corrupt("主机分组").with_source(source))?;
    if document.schema_version != GROUP_SCHEMA_VERSION {
        return Err(
            AppError::configuration_corrupt("主机分组").with_source(HostGroupDocumentInvalid)
        );
    }
    let mut ids = BTreeSet::new();
    for group in document.groups {
        let _preserved_fields = (
            group.name,
            group.sort_order,
            group.created_at,
            group.updated_at,
        );
        if !is_safe_group_id(&group.id) || !ids.insert(group.id) {
            return Err(
                AppError::configuration_corrupt("主机分组").with_source(HostGroupDocumentInvalid)
            );
        }
    }
    Ok(ids)
}

/// 在目录快照中拒绝 symlink、非法 TOML 文件名、坏 host 和悬空 group 引用。
fn read_host_documents(
    root: &Path,
    group_ids: &BTreeSet<String>,
) -> AppResult<BTreeMap<String, HostDocumentSnapshot>> {
    if !hosts_directory_exists(root)? {
        return Ok(BTreeMap::new());
    }
    let directory = root.join("hosts");

    let mut hosts = BTreeMap::new();
    let entries = fs::read_dir(&directory)
        .map_err(|source| AppError::infrastructure_with_source("读取主机目录失败", source))?;
    for entry in entries {
        let entry = entry
            .map_err(|source| AppError::infrastructure_with_source("枚举主机配置失败", source))?;
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new("toml"))
            || entry.file_name() == std::ffi::OsStr::new("groups.toml")
        {
            continue;
        }
        let file_type = entry.file_type().map_err(|source| {
            AppError::infrastructure_with_source("检查主机配置类型失败", source)
        })?;
        if !file_type.is_file() {
            return Err(
                AppError::configuration_corrupt("主机配置").with_source(HostGroupDocumentInvalid)
            );
        }
        if final_path_is_symlink(&path).map_err(|source| {
            AppError::infrastructure_with_source("检查主机配置类型失败", source)
        })? {
            return Err(
                AppError::configuration_corrupt("主机配置").with_source(HostGroupDocumentInvalid)
            );
        }
        let host_id = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| {
                AppError::configuration_corrupt("主机配置").with_source(HostGroupDocumentInvalid)
            })?
            .to_owned();
        let document_path = ConfigDocumentPath::Host(host_id.clone());
        let relative = relative_path(&document_path).map_err(|_| {
            AppError::configuration_corrupt("主机配置").with_source(HostGroupDocumentInvalid)
        })?;
        if portable_relative(&relative) != format!("hosts/{host_id}.toml") {
            return Err(
                AppError::configuration_corrupt("主机配置").with_source(HostGroupDocumentInvalid)
            );
        }
        let Some(source) = read_configuration_file(&path)
            .map_err(|source| AppError::infrastructure_with_source("读取主机配置失败", source))?
        else {
            return Err(AppError::conflict("主机配置在事务快照期间已变化"));
        };
        super::validation::validate_document(&document_path, &source)
            .map_err(|source| AppError::configuration_corrupt("主机配置").with_source(source))?;
        let value: toml::Value = toml::from_str(&source)
            .map_err(|source| AppError::configuration_corrupt("主机配置").with_source(source))?;
        let group_id = value
            .get("group_id")
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        if group_id
            .as_ref()
            .is_some_and(|group_id| !group_ids.contains(group_id))
        {
            return Err(
                AppError::configuration_corrupt("主机配置").with_source(HostGroupDocumentInvalid)
            );
        }
        let snapshot = HostDocumentSnapshot {
            revision: revision_for(&source),
            source,
            group_id,
        };
        if hosts.insert(host_id, snapshot).is_some() {
            return Err(
                AppError::configuration_corrupt("主机配置").with_source(HostGroupDocumentInvalid)
            );
        }
    }
    Ok(hosts)
}

/// 只有实际主机 ID 集合和每个原始 revision 完全相同时才允许分组迁移。
fn validate_complete_host_revision_set(
    actual: &BTreeMap<String, HostDocumentSnapshot>,
    expected_hosts: &[HostDocumentRevision],
) -> AppResult<()> {
    let mut expected = BTreeMap::new();
    for host in expected_hosts {
        if expected
            .insert(host.host_id.as_str(), host.revision.as_str())
            .is_some()
        {
            return Err(AppError::invalid("主机 revision 列表包含重复 ID"));
        }
    }
    if actual.len() != expected.len()
        || actual.iter().any(|(host_id, host)| {
            expected.get(host_id.as_str()).copied() != Some(host.revision.as_str())
        })
    {
        return Err(AppError::conflict("主机列表已变化，请刷新分组后重试"));
    }
    Ok(())
}

/// 保留 host 文件中的未知字段和注释，只变更 group_id 与更新时间。
fn rewrite_host_group(source: &str, destination_group_id: Option<&str>) -> AppResult<String> {
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|source| AppError::configuration_corrupt("主机配置").with_source(source))?;
    let table = document.as_table_mut();
    match destination_group_id {
        Some(group_id) => table["group_id"] = toml_edit::value(group_id),
        None => {
            table.remove("group_id");
        }
    }
    table["updated_at"] = toml_edit::value(unix_millis_string());
    Ok(document.to_string())
}

/// 只从 groups 数组移除目标项，避免重序列化或丢弃用户扩展字段。
fn remove_group_from_source(source: &str, group_id: &str) -> AppResult<String> {
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|source| AppError::configuration_corrupt("主机分组").with_source(source))?;
    let groups_item = document.as_table_mut().get_mut("groups").ok_or_else(|| {
        AppError::configuration_corrupt("主机分组").with_source(HostGroupDocumentInvalid)
    })?;
    if let Some(groups) = groups_item.as_array_of_tables_mut() {
        let index = groups
            .iter()
            .position(|group| group.get("id").and_then(Item::as_str) == Some(group_id))
            .ok_or_else(|| AppError::not_found("主机分组不存在"))?;
        groups.remove(index);
    } else if let Some(groups) = groups_item.as_value_mut().and_then(Value::as_array_mut) {
        let index = groups
            .iter()
            .position(|group| {
                group
                    .as_inline_table()
                    .and_then(|group| group.get("id"))
                    .and_then(Value::as_str)
                    == Some(group_id)
            })
            .ok_or_else(|| AppError::not_found("主机分组不存在"))?;
        groups.remove(index);
    } else {
        return Err(
            AppError::configuration_corrupt("主机分组").with_source(HostGroupDocumentInvalid)
        );
    }
    Ok(document.to_string())
}

/// 只允许主机事务在数据根下的真实 hosts 目录工作，不跟随目录符号链接。
fn hosts_directory_exists(root: &Path) -> AppResult<bool> {
    let directory = root.join("hosts");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => Ok(true),
        Ok(_) => {
            Err(AppError::configuration_corrupt("主机目录").with_source(HostGroupDocumentInvalid))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(AppError::infrastructure_with_source(
            "读取主机目录失败",
            source,
        )),
    }
}

/// 将固定目录相对路径规范为 journal 使用的正斜杠格式。
fn portable_relative(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// 原子建立 journal；任何已有文件或未知目录项都不能被覆盖。
fn write_host_group_journal(root: &Path, journal: &HostGroupJournal) -> AppResult<()> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|source| AppError::infrastructure_with_source("编码主机分组事务失败", source))?;
    if bytes.len() as u64 > MAX_HOST_GROUP_JOURNAL_BYTES {
        return Err(AppError::invalid("主机分组事务规模超过安全上限"));
    }
    let path = root.join(HOST_GROUP_JOURNAL);
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("主机分组事务路径无效"))?;
    fs::create_dir_all(parent).map_err(|source| {
        AppError::infrastructure_with_source("创建主机分组事务目录失败", source)
    })?;
    let metadata = fs::symlink_metadata(parent).map_err(|source| {
        AppError::infrastructure_with_source("检查主机分组事务目录失败", source)
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::configuration_corrupt("主机分组事务目录")
            .with_source(HostGroupJournalInvalid));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
            AppError::infrastructure_with_source("保护主机分组事务目录失败", source)
        })?;
    }
    atomic_replace(
        root,
        Path::new(HOST_GROUP_JOURNAL),
        &bytes,
        || match fs::symlink_metadata(&path) {
            Ok(_) => Err(AppError::configuration_corrupt("主机分组事务需要恢复")
                .with_source(HostGroupTransactionPending)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CommitMode::CreateNew),
            Err(source) => Err(AppError::infrastructure_with_source(
                "检查主机分组事务文件失败",
                source,
            )),
        },
    )
}

/// 校验 journal 版本、操作、白名单路径与 after 文档，再允许幂等重放。
fn read_host_group_journal(root: &Path) -> AppResult<Option<HostGroupJournal>> {
    let transaction_directory = root.join(".storage-transactions");
    match fs::symlink_metadata(&transaction_directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::configuration_corrupt("主机分组事务目录")
                .with_source(HostGroupJournalInvalid));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "检查主机分组事务目录失败",
                source,
            ));
        }
    }
    let path = root.join(HOST_GROUP_JOURNAL);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取主机分组事务状态失败",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_HOST_GROUP_JOURNAL_BYTES
    {
        return Err(AppError::configuration_corrupt("主机分组事务文件无效")
            .with_source(HostGroupJournalInvalid));
    }
    let mut file = fs::File::open(&path)
        .map_err(|source| AppError::infrastructure_with_source("读取主机分组事务失败", source))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_HOST_GROUP_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| AppError::infrastructure_with_source("读取主机分组事务失败", source))?;
    if bytes.len() as u64 > MAX_HOST_GROUP_JOURNAL_BYTES {
        return Err(AppError::configuration_corrupt("主机分组事务文件无效")
            .with_source(HostGroupJournalInvalid));
    }
    let journal: HostGroupJournal = serde_json::from_slice(&bytes).map_err(|source| {
        AppError::configuration_corrupt("主机分组事务文件无效").with_source(source)
    })?;
    validate_host_group_journal(&journal)?;
    Ok(Some(journal))
}

/// 校验 journal 的每个路径只落在 hosts 白名单，groups 必须最后提交。
fn validate_host_group_journal(journal: &HostGroupJournal) -> AppResult<()> {
    if journal.schema_version != 1
        || !is_safe_group_id(&journal.operation.deleted_group_id)
        || journal
            .operation
            .destination_group_id
            .as_deref()
            .is_some_and(|id| {
                !is_safe_group_id(id) || id == journal.operation.deleted_group_id.as_str()
            })
        || journal.entries.is_empty()
    {
        return Err(invalid_journal());
    }
    let mut paths = BTreeSet::new();
    let mut group_after = None;
    for (index, entry) in journal.entries.iter().enumerate() {
        if !is_sha256_hex(&entry.before_revision) || !paths.insert(entry.relative_path.as_str()) {
            return Err(invalid_journal());
        }
        let (relative, document_path) = journal_entry_path(&entry.relative_path)?;
        super::validation::validate_document(&document_path, &entry.after_source).map_err(
            |source| AppError::configuration_corrupt("主机分组事务文件无效").with_source(source),
        )?;
        match &document_path {
            ConfigDocumentPath::Host(id) => {
                if index + 1 == journal.entries.len() {
                    return Err(invalid_journal());
                }
                let host: toml::Value = toml::from_str(&entry.after_source).map_err(|source| {
                    AppError::configuration_corrupt("主机分组事务文件无效").with_source(source)
                })?;
                let after_group = host.get("group_id").and_then(toml::Value::as_str);
                if after_group != journal.operation.destination_group_id.as_deref()
                    || id.is_empty()
                    || portable_relative(&relative) != entry.relative_path
                {
                    return Err(invalid_journal());
                }
            }
            ConfigDocumentPath::HostGroups => {
                if index + 1 != journal.entries.len() || group_after.is_some() {
                    return Err(invalid_journal());
                }
                let groups = parse_group_ids(&entry.after_source)?;
                if groups.contains(&journal.operation.deleted_group_id)
                    || journal
                        .operation
                        .destination_group_id
                        .as_ref()
                        .is_some_and(|destination| !groups.contains(destination))
                {
                    return Err(invalid_journal());
                }
                group_after = Some(entry.after_source.as_str());
            }
            _ => return Err(invalid_journal()),
        }
    }
    if group_after.is_none() {
        return Err(invalid_journal());
    }
    Ok(())
}

/// Journal 只能命中主机文件或唯一 groups 文档，禁止任意相对路径。
fn journal_entry_path(relative: &str) -> AppResult<(PathBuf, ConfigDocumentPath)> {
    if relative == "hosts/groups.toml" {
        return Ok((
            PathBuf::from("hosts/groups.toml"),
            ConfigDocumentPath::HostGroups,
        ));
    }
    let id = relative
        .strip_prefix("hosts/")
        .and_then(|value| value.strip_suffix(".toml"))
        .filter(|value| !value.is_empty() && !value.contains('/') && !value.contains('\\'))
        .ok_or_else(invalid_journal)?;
    let document_path = ConfigDocumentPath::Host(id.to_owned());
    let path = relative_path(&document_path).map_err(|_| invalid_journal())?;
    if portable_relative(&path) != relative {
        return Err(invalid_journal());
    }
    Ok((path, document_path))
}

/// 对每个条目比较 before/after revision，提交尚未完成的文件并保留可重放 journal。
fn apply_host_group_journal_locked(root: &Path) -> AppResult<()> {
    let Some(journal) = read_host_group_journal(root)? else {
        return Ok(());
    };
    if !hosts_directory_exists(root)? {
        return Err(AppError::configuration_corrupt("主机分组事务目标无效")
            .with_source(HostGroupJournalInvalid));
    }
    for entry in &journal.entries {
        let (relative, document_path) = journal_entry_path(&entry.relative_path)?;
        let path = safe_join(root, &relative)?;
        if final_path_is_symlink(&path).map_err(|source| {
            AppError::infrastructure_with_source("检查主机分组事务目标失败", source)
        })? {
            return Err(AppError::configuration_corrupt("主机分组事务目标无效")
                .with_source(HostGroupJournalInvalid));
        }
        let current = read_configuration_file(&path).map_err(|source| {
            AppError::infrastructure_with_source("读取主机分组事务目标失败", source)
        })?;
        let Some(current) = current else {
            return Err(AppError::conflict("主机配置在分组事务期间已删除"));
        };
        let current_revision = revision_for(&current);
        let after_revision = revision_for(&entry.after_source);
        if current_revision == after_revision {
            continue;
        }
        if current_revision.as_str() != entry.before_revision {
            return Err(AppError::conflict("主机配置在分组事务期间被外部修改"));
        }
        super::validation::validate_document(&document_path, &current).map_err(|source| {
            AppError::configuration_corrupt("主机分组事务目标无效").with_source(source)
        })?;
        validate_journal_before_source(&journal.operation, &document_path, &current)?;
        super::validation::validate_document(&document_path, &entry.after_source).map_err(
            |source| AppError::configuration_corrupt("主机分组事务文件无效").with_source(source),
        )?;
        atomic_replace(root, &relative, entry.after_source.as_bytes(), || {
            if final_path_is_symlink(&path).map_err(|source| {
                AppError::infrastructure_with_source("检查主机分组事务目标失败", source)
            })? {
                return Err(AppError::conflict("符号链接配置不能由分组事务覆盖"));
            }
            let latest = read_configuration_file(&path).map_err(|source| {
                AppError::infrastructure_with_source("重新读取分组事务目标失败", source)
            })?;
            match latest.as_deref() {
                Some(source) if revision_for(source).as_str() == entry.before_revision => {
                    Ok(CommitMode::ReplaceExisting)
                }
                Some(source) if revision_for(source) == after_revision => {
                    Ok(CommitMode::ReplaceExisting)
                }
                _ => Err(AppError::conflict("主机配置在分组事务期间被外部修改")),
            }
        })?;
    }
    Ok(())
}

/// 恢复前确认 journal 的操作仍对应原 group 引用，避免损坏 journal 改写无关主机。
fn validate_journal_before_source(
    operation: &HostGroupOperation,
    path: &ConfigDocumentPath,
    source: &str,
) -> AppResult<()> {
    match path {
        ConfigDocumentPath::Host(_) => {
            let host: toml::Value = toml::from_str(source).map_err(|source| {
                AppError::configuration_corrupt("主机分组事务目标无效").with_source(source)
            })?;
            if host.get("group_id").and_then(toml::Value::as_str)
                != Some(operation.deleted_group_id.as_str())
            {
                return Err(AppError::conflict("主机配置与分组事务原状态不一致"));
            }
        }
        ConfigDocumentPath::HostGroups => {
            let group_ids = parse_group_ids(source)?;
            if !group_ids.contains(&operation.deleted_group_id)
                || operation
                    .destination_group_id
                    .as_ref()
                    .is_some_and(|destination| !group_ids.contains(destination))
            {
                return Err(AppError::conflict("主机分组与事务原状态不一致"));
            }
        }
        _ => return Err(invalid_journal()),
    }
    Ok(())
}

/// 删除事务 journal；Unix unlink 后目录同步失败明确保留 OutcomeUnknown。
fn remove_host_group_journal(root: &Path) -> AppResult<()> {
    let path = root.join(HOST_GROUP_JOURNAL);
    match remove_file_with_parent_sync(&path) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::NotCommitted(source)) => Err(AppError::infrastructure_with_source(
            "清理主机分组事务文件失败",
            source,
        )),
        Err(AtomicCommitError::CommitUnknown(source)) => {
            Err(AppError::outcome_unknown_with_source(
                "主机分组事务清理结果未知，请重新读取恢复状态",
                source,
            ))
        }
    }
}

/// 固定 groups schema 字段投影；serde 忽略未知扩展，实际写入仍保留源 TOML。
#[derive(Debug, Deserialize)]
struct GroupsFile {
    schema_version: u32,
    #[serde(default)]
    groups: Vec<GroupFile>,
}

/// 与 host 配置仓储的 groups 投影使用相同必填字段；未知键仍由源 TOML 保留。
#[derive(Debug, Deserialize)]
struct GroupFile {
    id: String,
    name: String,
    sort_order: i64,
    created_at: String,
    updated_at: String,
}

/// 单次分组迁移的操作摘要用于验证恢复目标，而不保存主机配置的旧秘密材料。
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostGroupJournal {
    schema_version: u32,
    operation: HostGroupOperation,
    entries: Vec<HostGroupJournalEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostGroupOperation {
    deleted_group_id: String,
    destination_group_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostGroupJournalEntry {
    relative_path: String,
    before_revision: String,
    after_source: String,
}

/// 拒绝未知事务原因并保留一个稳定内部分类供安全恢复状态使用。
fn invalid_journal() -> AppError {
    AppError::configuration_corrupt("主机分组事务文件无效").with_source(HostGroupJournalInvalid)
}

/// 分组 ID 与现有文件化配置使用相同的安全文件名字符集。
fn is_safe_group_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// Journal revision 只接受固定小写 SHA-256 十六进制格式。
fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug)]
struct HostGroupRecoveryFailure {
    original: AppError,
    recovery: AppError,
}

impl std::fmt::Display for HostGroupRecoveryFailure {
    /// 组合摘要保留失败阶段，但不显示源错误中可能包含的文件内容。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "分组事务初次应用错误码 {:?}，恢复错误码 {:?}",
            self.original.public_code(),
            self.recovery.public_code()
        )
    }
}

impl Error for HostGroupRecoveryFailure {
    /// 保留恢复错误作为 source，初次错误类别已安全写入摘要。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.recovery)
    }
}

#[derive(Debug)]
struct HostGroupDocumentInvalid;

impl std::fmt::Display for HostGroupDocumentInvalid {
    /// 只标识结构无效，不输出配置源或用户输入。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("主机分组文档结构无效")
    }
}

impl Error for HostGroupDocumentInvalid {}

#[derive(Debug)]
struct HostGroupTransactionPending;

impl std::fmt::Display for HostGroupTransactionPending {
    /// 表明既有事务 marker 需要恢复，错误不包含其内容。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("主机分组事务等待恢复")
    }
}

impl Error for HostGroupTransactionPending {}

#[derive(Debug)]
struct HostGroupJournalInvalid;

impl std::fmt::Display for HostGroupJournalInvalid {
    /// 表明 journal 路径或版本不受支持，不回显潜在敏感数据。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("主机分组事务 journal 无效")
    }
}

impl Error for HostGroupJournalInvalid {}

#[cfg(test)]
#[path = "../../tests/support/host_group_transaction_unit.rs"]
mod tests;
