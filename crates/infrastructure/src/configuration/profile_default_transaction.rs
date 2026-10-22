//! 默认 profile 跨文件提交、崩溃 journal 与幂等恢复协议。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::storage::{CommitMode, StorageCoordinator};
use kerminal_application::preferences::{ProfileRevision, VersionedProfile};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::preferences::ConfigDocumentPath;
use serde::{Deserialize, Serialize};

use super::{
    ConfigurationStore, ProfileTomlDocument, atomic_replace, final_path_is_symlink,
    read_configuration_file, relative_path, revision_for, safe_join, unix_millis_string,
};

impl ConfigurationStore {
    /// 在共享锁中校验整份 profile 列表，再用本地 journal 原子意图切换唯一默认项。
    ///
    /// @author kongweiguang
    pub(super) fn set_default_profile_document(
        &self,
        id: String,
        expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        self.ensure_writable()?;
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_writable()?;
        let journal_relative = profile_default_journal_path();
        let journal_path = safe_join(&self.inner.root, &journal_relative)?;
        if journal_path.exists() {
            return Err(AppError::configuration_corrupt("默认 profile 事务需要恢复"));
        }

        let profiles = self.list_versioned_profiles_document_unlocked()?;
        validate_profile_revision_set(&profiles, &expected_revisions)?;
        if !profiles.iter().any(|entry| entry.profile.id == id) {
            return Err(AppError::not_found("终端 profile 不存在"));
        }
        let mut entries = Vec::new();
        let updated_at = unix_millis_string();
        for versioned in &profiles {
            let desired_default = versioned.profile.id == id;
            if versioned.profile.is_default == desired_default {
                continue;
            }
            let relative =
                relative_path(&ConfigDocumentPath::Profile(versioned.profile.id.clone()))?;
            let absolute = safe_join(&self.inner.root, &relative)?;
            let source = read_configuration_file(&absolute)
                .map_err(|source| {
                    AppError::infrastructure_with_source("读取 profile 失败", source)
                })?
                .ok_or_else(|| AppError::conflict("profile 在默认切换期间已被删除"))?;
            if revision_for(&source) != versioned.revision {
                return Err(AppError::conflict(
                    "profile 已被其它操作修改，请重新读取后切换默认项",
                ));
            }
            let mut document = source.parse::<toml_edit::DocumentMut>().map_err(|source| {
                AppError::configuration_corrupt("终端 profile").with_source(source)
            })?;
            document["is_default"] = toml_edit::value(desired_default);
            document["updated_at"] = toml_edit::value(updated_at.clone());
            entries.push(ProfileDefaultJournalEntry {
                relative_path: relative.to_string_lossy().replace('\\', "/"),
                before_revision: versioned.revision.as_str().to_owned(),
                after_source: document.to_string(),
            });
        }
        if !entries.is_empty() {
            let journal = ProfileDefaultJournal {
                schema_version: 1,
                entries,
            };
            write_profile_default_journal(&self.inner.root, &journal)?;
            if let Err(original) = apply_profile_default_journal_locked(&self.inner.root)
                && let Err(recovery) = apply_profile_default_journal_locked(&self.inner.root)
            {
                return Err(AppError::outcome_unknown_with_source(
                    "默认 profile 切换结果未知，请重新读取配置确认",
                    ProfileDefaultRecoveryFailure { original, recovery },
                ));
            }
        }
        self.get_profile_document_unlocked(id)?
            .ok_or_else(|| AppError::not_found("终端 profile 不存在"))
    }
}

/// 固定本地事务目录承载可恢复写入意图，不让临时跨文件状态进入 Git 同步。
pub(super) fn profile_default_journal_path() -> PathBuf {
    PathBuf::from(".storage-transactions/profile-default.json")
}

/// 启动时用共享存储锁重放未完成的默认 profile 切换；所有文件修改都按 journal 目标 revision 幂等执行。
pub(super) fn recover_profile_default_transaction(
    root: &Path,
    storage: &StorageCoordinator,
) -> AppResult<()> {
    let _lock = storage
        .lock()
        .map_err(|source| AppError::infrastructure_with_source("无法锁定配置存储", source))?;
    apply_profile_default_journal_locked(root)
}

/// 在锁内拒绝一般配置读写穿过未完成 journal，确保恢复态由启动流程统一决定。
pub(super) fn ensure_profile_default_transaction_absent(root: &Path) -> AppResult<()> {
    let path = root.join(profile_default_journal_path());
    match fs::symlink_metadata(path) {
        Ok(_) => Err(AppError::configuration_corrupt("默认 profile 事务需要恢复")
            .with_source(ProfileDefaultTransactionPending)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AppError::infrastructure_with_source(
            "检查默认 profile 事务失败",
            source,
        )),
    }
}

/// 原子且 no-clobber 地建立 journal；目标已存在时不覆盖可能需要恢复的前一事务。
fn write_profile_default_journal(root: &Path, journal: &ProfileDefaultJournal) -> AppResult<()> {
    let bytes = serde_json::to_vec(journal).map_err(|source| {
        AppError::infrastructure_with_source("编码默认 profile 事务失败", source)
    })?;
    let journal_path = root.join(profile_default_journal_path());
    let parent = journal_path
        .parent()
        .ok_or_else(|| AppError::invalid("profile 事务路径无效"))?;
    fs::create_dir_all(parent).map_err(|source| {
        AppError::infrastructure_with_source("创建 profile 事务目录失败", source)
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
            AppError::infrastructure_with_source("保护 profile 事务目录失败", source)
        })?;
    }
    atomic_replace(root, &profile_default_journal_path(), &bytes, || {
        Ok(CommitMode::CreateNew)
    })
}

/// 读取并校验 journal 的版本、目标路径、原 revision 和全部写后 profile 文档。
fn read_profile_default_journal(root: &Path) -> AppResult<Option<ProfileDefaultJournal>> {
    let path = root.join(profile_default_journal_path());
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取默认 profile 事务状态失败",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::configuration_corrupt("默认 profile 事务文件无效")
            .with_source(ProfileDefaultJournalInvalid));
    }
    let bytes = fs::read(&path).map_err(|source| {
        AppError::infrastructure_with_source("读取默认 profile 事务失败", source)
    })?;
    let journal: ProfileDefaultJournal = serde_json::from_slice(&bytes).map_err(|source| {
        AppError::configuration_corrupt("默认 profile 事务文件无效").with_source(source)
    })?;
    if journal.schema_version != 1 || journal.entries.is_empty() {
        return Err(AppError::configuration_corrupt("默认 profile 事务文件无效")
            .with_source(ProfileDefaultJournalInvalid));
    }

    let mut seen = std::collections::BTreeSet::new();
    for entry in &journal.entries {
        let (relative, document_path) = profile_journal_entry_path(&entry.relative_path)?;
        if !seen.insert(entry.relative_path.as_str()) || !is_sha256_hex(&entry.before_revision) {
            return Err(AppError::configuration_corrupt("默认 profile 事务文件无效")
                .with_source(ProfileDefaultJournalInvalid));
        }
        super::validation::validate_document(&document_path, &entry.after_source).map_err(
            |source| {
                AppError::configuration_corrupt("默认 profile 事务文件无效").with_source(source)
            },
        )?;
        let profile: ProfileTomlDocument =
            toml::from_str(&entry.after_source).map_err(|source| {
                AppError::configuration_corrupt("默认 profile 事务文件无效").with_source(source)
            })?;
        if super::validation::document_id(&document_path) != Some(profile.id.as_str())
            || relative_path(&document_path).ok().as_deref() != Some(relative.as_path())
        {
            return Err(AppError::configuration_corrupt("默认 profile 事务文件无效")
                .with_source(ProfileDefaultJournalInvalid));
        }
    }
    Ok(Some(journal))
}

/// 只接受 journal 中由应用生成的 `profiles/<id>.toml` 路径，拒绝绝对路径和平台分隔符变体。
fn profile_journal_entry_path(relative: &str) -> AppResult<(PathBuf, ConfigDocumentPath)> {
    let id = relative
        .strip_prefix("profiles/")
        .and_then(|value| value.strip_suffix(".toml"))
        .filter(|id| !id.is_empty() && !id.contains('/') && !id.contains('\\'))
        .ok_or_else(|| {
            AppError::configuration_corrupt("默认 profile 事务路径无效")
                .with_source(ProfileDefaultJournalInvalid)
        })?;
    let document_path = ConfigDocumentPath::Profile(id.to_owned());
    let safe_path = relative_path(&document_path).map_err(|_| {
        AppError::configuration_corrupt("默认 profile 事务路径无效")
            .with_source(ProfileDefaultJournalInvalid)
    })?;
    if safe_path.to_string_lossy().replace('\\', "/") != relative {
        return Err(AppError::configuration_corrupt("默认 profile 事务路径无效")
            .with_source(ProfileDefaultJournalInvalid));
    }
    Ok((safe_path, document_path))
}

/// 对 journal 所有 profile 逐项比较 before/after revision，使崩溃后重复恢复保持幂等。
fn apply_profile_default_journal_locked(root: &Path) -> AppResult<()> {
    let Some(journal) = read_profile_default_journal(root)? else {
        return Ok(());
    };
    for entry in &journal.entries {
        let (relative, _) = profile_journal_entry_path(&entry.relative_path)?;
        let path = safe_join(root, &relative)?;
        let current = read_configuration_file(&path)
            .map_err(|source| {
                AppError::infrastructure_with_source("恢复 profile 事务失败", source)
            })?
            .ok_or_else(|| AppError::conflict("profile 在默认切换事务期间已被删除"))?;
        let current_revision = revision_for(&current);
        let expected_after = revision_for(&entry.after_source);
        if current_revision == expected_after {
            continue;
        }
        if current_revision.as_str() != entry.before_revision {
            return Err(AppError::conflict("profile 在默认切换事务期间发生外部修改"));
        }
        atomic_replace(root, &relative, entry.after_source.as_bytes(), || {
            if final_path_is_symlink(&path).map_err(|source| {
                AppError::infrastructure_with_source("检查 profile 事务目标失败", source)
            })? {
                return Err(AppError::conflict("符号链接 profile 不能由默认事务覆盖"));
            }
            let latest = read_configuration_file(&path).map_err(|source| {
                AppError::infrastructure_with_source("重新读取 profile 事务目标失败", source)
            })?;
            match latest.as_deref() {
                Some(source) if revision_for(source).as_str() == entry.before_revision => {
                    Ok(CommitMode::ReplaceExisting)
                }
                Some(source) if revision_for(source) == expected_after => {
                    Ok(CommitMode::ReplaceExisting)
                }
                _ => Err(AppError::conflict("profile 在默认切换事务期间发生外部修改")),
            }
        })?;
    }

    let journal_path = root.join(profile_default_journal_path());
    if read_profile_default_journal(root)?.as_ref() != Some(&journal) {
        return Err(AppError::conflict("profile 默认事务状态已变化，请重启恢复"));
    }
    if final_path_is_symlink(&journal_path).map_err(|source| {
        AppError::infrastructure_with_source("检查 profile 事务文件失败", source)
    })? {
        return Err(AppError::configuration_corrupt("默认 profile 事务文件无效")
            .with_source(ProfileDefaultJournalInvalid));
    }
    fs::remove_file(&journal_path).map_err(|source| {
        AppError::infrastructure_with_source("清理默认 profile 事务失败", source)
    })?;
    sync_parent_directory(&journal_path).map_err(|source| {
        AppError::infrastructure_with_source("同步默认 profile 事务目录失败", source)
    })?;
    Ok(())
}

/// 校验切换默认项使用的列表快照完整且无重复，防止漏传 profile 绕开整体 CAS。
fn validate_profile_revision_set(
    profiles: &[VersionedProfile],
    expected_revisions: &[ProfileRevision],
) -> AppResult<()> {
    if profiles.len() != expected_revisions.len() {
        return Err(AppError::conflict(
            "profile 列表已变化，请重新读取后切换默认项",
        ));
    }
    let mut expected = BTreeMap::new();
    for item in expected_revisions {
        if expected
            .insert(item.profile_id.as_str(), item.revision.as_str())
            .is_some()
        {
            return Err(AppError::invalid("profile revision 列表包含重复 ID"));
        }
    }
    if profiles
        .iter()
        .all(|item| expected.get(item.profile.id.as_str()).copied() == Some(item.revision.as_str()))
    {
        Ok(())
    } else {
        Err(AppError::conflict(
            "profile 列表已变化，请重新读取后切换默认项",
        ))
    }
}

/// 验证持久化 revision 仅包含固定长度的小写十六进制摘要。
fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 删除事务 journal 后同步父目录；目录同步不适用的平台由文件系统提交原语负责。
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal has no parent directory",
            )
        })?;
        fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// 默认 profile 跨文件切换的恢复意图；该本地 journal 不属于可同步用户配置。
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileDefaultJournal {
    schema_version: u32,
    entries: Vec<ProfileDefaultJournalEntry>,
}

/// 每个 journal 条目绑定原 revision 与完整目标内容，重复重放时可识别已提交文件。
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileDefaultJournalEntry {
    relative_path: String,
    before_revision: String,
    after_source: String,
}

/// 保留默认 profile 两阶段失败的原始分类，公开错误本身仍使用固定配置恢复文案。
#[derive(Debug)]
struct ProfileDefaultRecoveryFailure {
    original: AppError,
    recovery: AppError,
}

impl std::fmt::Display for ProfileDefaultRecoveryFailure {
    /// 只公开恢复摘要，内部原始原因留在字段中供诊断器检查。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = (&self.original, &self.recovery);
        formatter.write_str("profile default transaction could not be recovered")
    }
}

impl Error for ProfileDefaultRecoveryFailure {
    /// 最近一次重放失败原因作为标准 source；首次提交失败仍保留在该对象中。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.recovery)
    }
}

/// 当前进程发现 journal 时拒绝穿过事务继续读写，直到启动恢复成功。
#[derive(Debug)]
struct ProfileDefaultTransactionPending;

impl std::fmt::Display for ProfileDefaultTransactionPending {
    /// 返回固定摘要，不暴露 journal 原文或配置根路径。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("profile default transaction is pending recovery")
    }
}

impl Error for ProfileDefaultTransactionPending {}

/// Journal 结构、版本或绑定路径不符合本地恢复协议时的固定错误源。
#[derive(Debug)]
struct ProfileDefaultJournalInvalid;

impl std::fmt::Display for ProfileDefaultJournalInvalid {
    /// 只报告协议无效，不回显受损 journal 字段。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("profile default transaction journal is invalid")
    }
}

impl Error for ProfileDefaultJournalInvalid {}
