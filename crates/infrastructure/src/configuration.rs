//! `~/.kerminal` 文件化配置仓储与启动恢复门禁。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use kerminal_application::configuration_changes::{
    ConfigurationChangeEvent, ConfigurationChangeRepository,
};
use kerminal_application::preferences::{
    CONFIG_GUIDE, CONFIG_GUIDE_REVISION, ConfigurationGuideStatus, ConfigurationRepository,
    ConfigurationValidationReport, ConfigurationValidationRepository, ConfigurationValidationScope,
    ProfileDraft, ProfileRepository, ProfileRevision, SettingsRepository, SettingsSnapshot,
    VersionedProfile, VersionedSettings,
};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::preferences::{
    ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationIssue, ConfigurationIssueKind,
    ConfigurationMode, ConfigurationRecoveryState, TerminalProfile,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio::task;

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

mod host_group_transaction;
mod profile_default_transaction;
mod settings;
mod validation;

pub(crate) const CONFIG_GUIDE_MARKER_PREFIX: &str =
    "<!-- kerminal-generated-config-guide revision=";

/// 一个进程共享的配置文件仓储；启动恢复模式一经确定，在进程退出前不可被 UI 绕过。
#[derive(Clone)]
pub struct ConfigurationStore {
    inner: Arc<ConfigurationStoreInner>,
}

struct ConfigurationStoreInner {
    root: PathBuf,
    recovery: RwLock<ConfigurationRecoveryState>,
    changes: broadcast::Sender<ConfigurationChangeEvent>,
    guide_status: ConfigurationGuideStatus,
    storage: StorageCoordinator,
}

/// 用户数据根不可用时返回启动错误；源错误保留给桌面启动诊断，文案不含绝对路径。
#[derive(Debug)]
pub struct ConfigurationStoreInitError {
    source: io::Error,
}

impl ConfigurationStore {
    /// 创建用户数据根并恢复 profile、host/group journal；损坏文件保留原文并阻断写入。
    pub fn initialize(root: impl AsRef<Path>) -> Result<Self, ConfigurationStoreInitError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|source| ConfigurationStoreInitError { source })?;
        let storage = StorageCoordinator::new(root.clone());
        let guide_status = ensure_config_guide(&root, &storage);
        let profile_transaction_recovery =
            profile_default_transaction::recover_profile_default_transaction(&root, &storage);
        let host_group_transaction_recovery =
            host_group_transaction::recover_host_group_transaction(&root, &storage);
        let mut recovery = validation::inspect_startup_configuration(&root);
        if profile_transaction_recovery.is_err() {
            recovery.issues.push(unreadable_issue(
                ".storage-transactions/profile-default.json".to_owned(),
            ));
        }
        if host_group_transaction_recovery.is_err() {
            recovery.issues.push(unreadable_issue(
                ".storage-transactions/host-group.json".to_owned(),
            ));
        }
        if profile_transaction_recovery.is_err() || host_group_transaction_recovery.is_err() {
            recovery
                .issues
                .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            recovery.mode = ConfigurationMode::ReadOnlyRecovery;
        }
        let (changes, _initial_receiver) = broadcast::channel(64);
        Ok(Self {
            inner: Arc::new(ConfigurationStoreInner {
                root,
                recovery: RwLock::new(recovery),
                changes,
                guide_status,
                storage,
            }),
        })
    }

    /// 暴露同一个已解析根目录，供 vault、SQLite 与其它文件 adapter 共用路径布局。
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// 返回安全可展示的启动恢复状态，不泄露原始 TOML、字段值或根目录绝对路径。
    pub fn recovery_state(&self) -> ConfigurationRecoveryState {
        match self.inner.recovery.read() {
            Ok(state) => state.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 外部 watcher 在共享存储锁内重扫配置；只发布相对路径和安全恢复摘要，不发送文件内容。
    pub fn refresh_external_changes(
        &self,
        changed_paths: Vec<String>,
        full_rescan_required: bool,
    ) -> AppResult<ConfigurationRecoveryState> {
        let lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        let mut recovery = validation::inspect_startup_configuration(&self.inner.root);
        match fs::symlink_metadata(
            self.inner
                .root
                .join(profile_default_transaction::profile_default_journal_path()),
        ) {
            Ok(_) => {
                recovery.issues.push(unreadable_issue(
                    ".storage-transactions/profile-default.json".to_owned(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => recovery.issues.push(unreadable_issue(
                ".storage-transactions/profile-default.json".to_owned(),
            )),
        }
        match fs::symlink_metadata(
            self.inner
                .root
                .join(".storage-transactions/host-group.json"),
        ) {
            Ok(_) => recovery.issues.push(unreadable_issue(
                ".storage-transactions/host-group.json".to_owned(),
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => recovery.issues.push(unreadable_issue(
                ".storage-transactions/host-group.json".to_owned(),
            )),
        }
        recovery
            .issues
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        if !recovery.issues.is_empty() {
            recovery.mode = ConfigurationMode::ReadOnlyRecovery;
        }
        let snapshot = recovery.clone();
        match self.inner.recovery.write() {
            Ok(mut current) => *current = recovery,
            Err(poisoned) => *poisoned.into_inner() = recovery,
        }
        drop(lock);

        let event = ConfigurationChangeEvent {
            relative_paths: changed_paths
                .iter()
                .map(|path| safe_relative_path(Path::new(path)))
                .filter(|path| !path.is_empty())
                .collect(),
            recovery_state: snapshot.clone(),
            full_rescan_required,
        };
        let _ = self.inner.changes.send(event);
        Ok(snapshot)
    }

    /// 在获取存储锁之后重新检查当前恢复态，防止 watcher 刚发现损坏时仍写入配置。
    fn ensure_writable(&self) -> AppResult<()> {
        if self.recovery_state().mode == ConfigurationMode::ReadOnlyRecovery {
            return Err(AppError::configuration_corrupt("文件化配置")
                .with_source(ConfigurationWriteBlocked));
        }
        Ok(())
    }

    /// 普通配置路径统一检查 profile 与 host/group journal，避免任一事务半完成时读写穿透。
    fn ensure_configuration_transactions_absent(&self) -> AppResult<()> {
        profile_default_transaction::ensure_profile_default_transaction_absent(&self.inner.root)?;
        host_group_transaction::ensure_host_group_transaction_absent(&self.inner.root)
    }

    /// 返回启动时指南更新结果，桌面可以展示提示而不把辅助文档写入恢复门禁。
    pub fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        self.inner.guide_status
    }

    /// 返回相同进程互斥和锁文件的 clone，供 vault、SQLite 与文件仓储共享写入边界。
    pub fn storage_coordinator(&self) -> StorageCoordinator {
        self.inner.storage.clone()
    }

    /// 将文件读取放到 Tokio 阻塞池，避免同步磁盘 I/O 占用异步网络工作线程。
    async fn read_blocking(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        let store = self.clone();
        task::spawn_blocking(move || store.read_document(path))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("读取配置任务意外终止", source)
            })?
    }

    /// 在共享存储锁内读取原始字节并计算 revision，保留未知 TOML 字段供编辑器原样保存。
    fn read_document(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_configuration_transactions_absent()?;
        self.read_document_unlocked(path)
    }

    /// 在共享锁内执行只读配置 validator；坏文档作为报告返回，不触发写入或恢复覆盖。
    fn validate_configuration_document(
        &self,
        scope: ConfigurationValidationScope,
    ) -> AppResult<ConfigurationValidationReport> {
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_configuration_transactions_absent()?;
        validation::validate_configuration_root(&self.inner.root, scope)
    }

    /// 调用方已持有共享存储锁时读取单份原始文档，避免递归取得同一把锁。
    fn read_document_unlocked(
        &self,
        path: ConfigDocumentPath,
    ) -> AppResult<Option<ConfigDocument>> {
        let relative_path = relative_path(&path)?;
        let absolute_path = safe_join(&self.inner.root, &relative_path)?;
        let Some(source) = read_configuration_file(&absolute_path)
            .map_err(|source| AppError::infrastructure_with_source("读取配置文档失败", source))?
        else {
            return Ok(None);
        };
        Ok(Some(ConfigDocument {
            path,
            revision: revision_for(&source),
            source,
        }))
    }

    /// 只解码启动恢复态允许列出的 profile；坏文件由恢复报告定位，不以临时默认值覆盖。
    fn list_profiles_document(&self) -> AppResult<Vec<TerminalProfile>> {
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_configuration_transactions_absent()?;
        self.list_profiles_document_unlocked()
    }

    /// 在调用方持有共享锁时枚举 profile 文件，提供同一事务中的一致视图。
    fn list_profiles_document_unlocked(&self) -> AppResult<Vec<TerminalProfile>> {
        self.ensure_configuration_transactions_absent()?;
        let directory = self.inner.root.join("profiles");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(AppError::infrastructure_with_source(
                    "读取终端 profile 目录失败",
                    error,
                ));
            }
        };

        let mut profiles = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| {
                AppError::infrastructure_with_source("读取终端 profile 文件列表失败", source)
            })?;
            let path = entry.path();
            if path.extension() != Some(OsStr::new("toml")) {
                continue;
            }
            let file_type = entry.file_type().map_err(|source| {
                AppError::infrastructure_with_source("检查终端 profile 文件失败", source)
            })?;
            // 与现行目录枚举一致：常规列表忽略符号链接条目；按 ID 的直接读取仍可跟随链接。
            if !file_type.is_file() {
                continue;
            }
            let Some(profile_id) = path.file_stem().and_then(OsStr::to_str) else {
                continue;
            };
            let relative = PathBuf::from("profiles").join(entry.file_name());
            if self.recovery_state().issues.iter().any(|issue| {
                issue.relative_path == safe_relative_path(&relative)
                    || issue.relative_path == "profiles"
            }) {
                continue;
            }
            let source = read_configuration_file(&path)
                .map_err(|source| {
                    AppError::infrastructure_with_source("读取终端 profile 失败", source)
                })?
                .ok_or_else(|| AppError::not_found("终端 profile 不存在"))?;
            validation::validate_document(
                &ConfigDocumentPath::Profile(profile_id.to_owned()),
                &source,
            )
            .map_err(|source| {
                AppError::configuration_corrupt("终端 profile").with_source(source)
            })?;
            let document: ProfileTomlDocument = toml::from_str(&source).map_err(|source| {
                AppError::configuration_corrupt("终端 profile").with_source(source)
            })?;
            if document.id != profile_id {
                return Err(AppError::configuration_corrupt("终端 profile")
                    .with_source(ProfilePathMismatch));
            }
            profiles.push(document.into_profile());
        }
        profiles.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(profiles)
    }

    /// 为配置页读取每份 profile 的原始字节 revision，列表仍按磁盘稳定排序。
    fn list_versioned_profiles_document(&self) -> AppResult<Vec<VersionedProfile>> {
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_configuration_transactions_absent()?;
        self.list_versioned_profiles_document_unlocked()
    }

    /// 从已持锁的目录快照构造 profile revision 列表。
    fn list_versioned_profiles_document_unlocked(&self) -> AppResult<Vec<VersionedProfile>> {
        let profiles = self.list_profiles_document_unlocked()?;
        profiles
            .into_iter()
            .map(|profile| {
                let document = self
                    .read_document_unlocked(ConfigDocumentPath::Profile(profile.id.clone()))?
                    .ok_or_else(|| AppError::not_found("终端 profile 不存在"))?;
                Ok(VersionedProfile {
                    profile,
                    revision: document.revision,
                })
            })
            .collect()
    }

    /// 读取单份 profile，统一执行 schema、ID/文件名一致性和只读恢复状态检查。
    fn get_profile_document(&self, id: String) -> AppResult<Option<VersionedProfile>> {
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_configuration_transactions_absent()?;
        self.get_profile_document_unlocked(id)
    }

    /// 在调用方持有共享锁时解析单份 profile 文档。
    fn get_profile_document_unlocked(&self, id: String) -> AppResult<Option<VersionedProfile>> {
        let Some(document) =
            self.read_document_unlocked(ConfigDocumentPath::Profile(id.clone()))?
        else {
            return Ok(None);
        };
        validation::validate_document(&document.path, &document.source).map_err(|source| {
            AppError::configuration_corrupt("终端 profile").with_source(source)
        })?;
        let decoded: ProfileTomlDocument = toml::from_str(&document.source).map_err(|source| {
            AppError::configuration_corrupt("终端 profile").with_source(source)
        })?;
        if decoded.id != id {
            return Err(
                AppError::configuration_corrupt("终端 profile").with_source(ProfilePathMismatch)
            );
        }
        Ok(Some(VersionedProfile {
            profile: decoded.into_profile(),
            revision: document.revision,
        }))
    }

    /// 在原 TOML 上仅替换 schema 1 已知字段，未知键和注释由 toml_edit 留在原文中。
    fn save_profile_document(
        &self,
        draft: ProfileDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        let id = draft
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let path = ConfigDocumentPath::Profile(id.clone());
        let current = self.read_document(path.clone())?;
        let existing = match (expected_revision.as_ref(), current.as_ref()) {
            (None, None) => None,
            (Some(expected), Some(document)) if expected == &document.revision => {
                let decoded: ProfileTomlDocument =
                    toml::from_str(&document.source).map_err(|source| {
                        AppError::configuration_corrupt("终端 profile").with_source(source)
                    })?;
                if decoded.id != id {
                    return Err(AppError::configuration_corrupt("终端 profile")
                        .with_source(ProfilePathMismatch));
                }
                Some(decoded)
            }
            _ => {
                return Err(AppError::conflict(
                    "profile 已被其它操作修改，请重新读取后保存",
                ));
            }
        };
        let timestamp = unix_millis_string();
        let payload = ProfileTomlWrite {
            schema_version: 1,
            id: id.clone(),
            name: draft.name,
            shell: draft.shell,
            args: draft.args,
            cwd: draft.cwd,
            env: draft.env,
            is_default: existing.as_ref().is_some_and(|profile| profile.is_default),
            sidebar_group_id: draft.sidebar_group_id,
            sort_order: draft.sort_order,
            created_at: existing
                .as_ref()
                .map(|profile| profile.created_at.clone())
                .unwrap_or_else(|| timestamp.clone()),
            updated_at: timestamp,
        };
        let mut updated = match current {
            Some(document) => {
                document
                    .source
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|source| {
                        AppError::configuration_corrupt("终端 profile").with_source(source)
                    })?
            }
            None => toml_edit::DocumentMut::new(),
        };
        let replacement = toml_edit::ser::to_document(&payload)
            .map_err(|source| AppError::invalid("profile 字段无法编码").with_source(source))?;
        for key in [
            "schema_version",
            "id",
            "name",
            "shell",
            "args",
            "cwd",
            "env",
            "is_default",
            "sidebar_group_id",
            "sort_order",
            "created_at",
            "updated_at",
        ] {
            if let Some(item) = replacement.get(key) {
                updated[key] = item.clone();
            } else {
                updated.remove(key);
            }
        }
        let source = updated.to_string();
        let revision = self.write_document(path, source, expected_revision)?;
        let profile = payload.into_profile();
        Ok(VersionedProfile { profile, revision })
    }

    /// 在共享锁和 CAS 下删除单个普通文件；最终 symlink 不会被跟随或误删。
    fn delete_profile_document(&self, id: String, expected: ConfigRevision) -> AppResult<bool> {
        self.ensure_writable()?;
        let relative = relative_path(&ConfigDocumentPath::Profile(id))?;
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_writable()?;
        self.ensure_configuration_transactions_absent()?;
        let path = safe_join(&self.inner.root, &relative)?;
        if final_path_is_symlink(&path)
            .map_err(|source| AppError::infrastructure_with_source("检查配置目标失败", source))?
        {
            return Err(AppError::conflict("符号链接 profile 可以读取，但不能删除"));
        }
        let Some(source) = read_configuration_file(&path)
            .map_err(|source| AppError::infrastructure_with_source("读取 profile 失败", source))?
        else {
            return Ok(false);
        };
        if expected.as_str() != revision_for(&source).as_str() {
            return Err(AppError::conflict(
                "profile 已被其它操作修改，请重新读取后删除",
            ));
        }
        fs::remove_file(path)
            .map_err(|source| AppError::infrastructure_with_source("删除 profile 失败", source))?;
        Ok(true)
    }

    /// 校验快照与打开时 revision，在原 TOML 上 patch 后交回配置根的原子写入门禁。
    fn save_settings_document(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        self.ensure_writable()?;
        let settings = settings.validated()?;
        let current = self.read_document(ConfigDocumentPath::Settings)?;
        match (expected_revision.as_ref(), current.as_ref()) {
            (None, None) => {}
            (Some(expected), Some(document)) if expected == &document.revision => {}
            _ => return Err(AppError::conflict("设置已被其它操作修改，请重新读取后保存")),
        }
        let source = settings::patch_settings_document(
            current.as_ref().map(|document| document.source.as_str()),
            &settings,
        )?;
        let revision =
            self.write_document(ConfigDocumentPath::Settings, source, expected_revision)?;
        Ok(VersionedSettings {
            settings,
            revision: Some(revision),
        })
    }

    /// 在唯一存储锁和 compare-and-swap revision 保护下原子替换一份配置。
    fn write_document(
        &self,
        path: ConfigDocumentPath,
        source: String,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        self.ensure_writable()?;
        validation::validate_document(&path, &source)
            .map_err(|error| AppError::configuration_corrupt("配置文档").with_source(error))?;
        let relative_path = relative_path(&path)?;
        let _lock =
            self.inner.storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定配置存储", source)
            })?;
        self.ensure_writable()?;
        self.ensure_configuration_transactions_absent()?;
        let absolute_path = safe_join(&self.inner.root, &relative_path)?;

        if final_path_is_symlink(&absolute_path)
            .map_err(|source| AppError::infrastructure_with_source("检查配置目标失败", source))?
        {
            return Err(AppError::conflict(
                "符号链接配置可以读取，但不能通过编辑器覆盖",
            ));
        }
        let current_source = read_configuration_file(&absolute_path)
            .map_err(|source| AppError::infrastructure_with_source("读取配置文档失败", source))?;
        match (expected_revision.as_ref(), current_source.as_deref()) {
            (None, None) => {}
            (Some(expected), Some(current))
                if expected.as_str() == revision_for(current).as_str() => {}
            _ => return Err(AppError::conflict("配置已被其它操作修改，请重新读取后保存")),
        }

        atomic_replace(&self.inner.root, &relative_path, source.as_bytes(), || {
            if final_path_is_symlink(&absolute_path).map_err(|source| {
                AppError::infrastructure_with_source("检查配置目标失败", source)
            })? {
                return Err(AppError::conflict(
                    "符号链接配置可以读取，但不能通过编辑器覆盖",
                ));
            }
            let latest = read_configuration_file(&absolute_path).map_err(|source| {
                AppError::infrastructure_with_source("重新读取配置文档失败", source)
            })?;
            match (expected_revision.as_ref(), latest.as_deref()) {
                (None, None) => Ok(CommitMode::CreateNew),
                (Some(expected), Some(current))
                    if expected.as_str() == revision_for(current).as_str() =>
                {
                    Ok(CommitMode::ReplaceExisting)
                }
                _ => Err(AppError::conflict("配置已被其它操作修改，请重新读取后保存")),
            }
        })?;
        Ok(revision_for(&source))
    }
}

impl ConfigurationChangeRepository for ConfigurationStore {
    /// 为外部配置监听的每位消费方创建独立接收器，不在订阅时读取磁盘。
    fn subscribe_changes(&self) -> broadcast::Receiver<ConfigurationChangeEvent> {
        self.inner.changes.subscribe()
    }

    /// 读取由 watcher 更新的安全恢复态，lag 后可直接修正页面写入门禁。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationStore::recovery_state(self)
    }
}

#[async_trait::async_trait]
impl SettingsRepository for ConfigurationStore {
    /// 把文件读取、revision 计算和 TOML 解码放在同一阻塞池任务中完成。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let store = self.clone();
        task::spawn_blocking(move || {
            let Some(document) = store.read_document(ConfigDocumentPath::Settings)? else {
                return Ok(VersionedSettings {
                    settings: SettingsSnapshot::default(),
                    revision: None,
                });
            };
            validation::validate_document(&document.path, &document.source)
                .map_err(|source| AppError::configuration_corrupt("设置").with_source(source))?;
            let settings = settings::decode_settings(&document.source)
                .map_err(|source| AppError::configuration_corrupt("设置").with_source(source))?;
            Ok(VersionedSettings {
                settings: settings.validated()?,
                revision: Some(document.revision),
            })
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("读取设置任务意外终止", source))?
    }

    /// 由同一个配置仓储读取旧文档、局部 patch 并通过原子 CAS 提交。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        let store = self.clone();
        task::spawn_blocking(move || store.save_settings_document(settings, expected_revision))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("保存设置任务意外终止", source)
            })?
    }
}

#[async_trait::async_trait]
impl ConfigurationRepository for ConfigurationStore {
    /// 启动时固定恢复态，保证 UI 与 MCP 使用同一写入门禁。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationStore::recovery_state(self)
    }

    /// 把生成文档的更新状态传给 UI，确保自定义指南被保留时用户能看到原因。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationStore::configuration_guide_status(self)
    }

    /// 读取配置文档原文和内容 revision，编辑器可以保留未知字段。
    async fn read(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        self.read_blocking(path).await
    }

    /// 通过阻塞池执行解析、锁与原子文件替换，避免磁盘操作阻塞 Tokio runtime。
    async fn write(
        &self,
        path: ConfigDocumentPath,
        source: String,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        let store = self.clone();
        task::spawn_blocking(move || store.write_document(path, source, expected_revision))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("保存配置任务意外终止", source)
            })?
    }
}

#[async_trait::async_trait]
impl ProfileRepository for ConfigurationStore {
    /// 在 Tokio 阻塞池扫描 schema 1 profile，保持 UI 不接触磁盘格式与平台 shell 规则。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        let store = self.clone();
        task::spawn_blocking(move || store.list_profiles_document())
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("读取 profile 任务意外终止", source)
            })?
    }

    /// 返回配置页所需的 profile/revision 对，阻塞扫描放到 Tokio 专用阻塞池执行。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        let store = self.clone();
        task::spawn_blocking(move || store.list_versioned_profiles_document())
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("读取 profile 列表任务意外终止", source)
            })?
    }

    /// 通过应用 ID 映射读取 profile，绝不让调用方传入任意相对路径。
    async fn get_profile(&self, id: &str) -> AppResult<Option<VersionedProfile>> {
        let store = self.clone();
        let id = id.to_owned();
        task::spawn_blocking(move || store.get_profile_document(id))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("读取 profile 任务意外终止", source)
            })?
    }

    /// 通过 toml_edit 保留 profile 未知字段和注释，并由原子仓储执行 CAS。
    async fn save_profile(
        &self,
        draft: ProfileDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        let store = self.clone();
        task::spawn_blocking(move || store.save_profile_document(draft, expected_revision))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("保存 profile 任务意外终止", source)
            })?
    }

    /// 删除只针对 revision 未变化的普通 profile 文件，防止跨编辑器丢失修改。
    async fn delete_profile(&self, id: &str, expected_revision: ConfigRevision) -> AppResult<bool> {
        let store = self.clone();
        let id = id.to_owned();
        task::spawn_blocking(move || store.delete_profile_document(id, expected_revision))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("删除 profile 任务意外终止", source)
            })?
    }

    /// 在单一阻塞事务中切换默认标记，跨文件提交失败时用本地 journal 恢复到一致状态。
    async fn set_default_profile(
        &self,
        id: &str,
        expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        let store = self.clone();
        let id = id.to_owned();
        task::spawn_blocking(move || store.set_default_profile_document(id, expected_revisions))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("切换默认 profile 任务意外终止", source)
            })?
    }
}

#[async_trait::async_trait]
impl ConfigurationValidationRepository for ConfigurationStore {
    /// 将配置扫描和 TOML 解析交给 Tokio 阻塞池，validator 本身始终只读。
    async fn validate_configuration(
        &self,
        scope: ConfigurationValidationScope,
    ) -> AppResult<ConfigurationValidationReport> {
        let store = self.clone();
        task::spawn_blocking(move || store.validate_configuration_document(scope))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("配置校验任务意外终止", source)
            })?
    }
}

impl std::fmt::Display for ConfigurationStoreInitError {
    /// 只输出稳定启动错误，不把配置根路径暴露给 IPC 或 UI 文案。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("无法初始化用户配置目录")
    }
}

impl Error for ConfigurationStoreInitError {
    /// 保留底层目录创建原因供桌面内部诊断。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// 新启动只读取当前生成指南或播种缺失文件；历史正文交由独立迁移 planner 处理。
///
/// @author kongweiguang
fn ensure_config_guide(root: &Path, storage: &StorageCoordinator) -> ConfigurationGuideStatus {
    let destination = root.join("kerminal-config.md");
    let Ok(_lock) = storage.lock() else {
        return ConfigurationGuideStatus::Unavailable;
    };
    let metadata = match fs::symlink_metadata(&destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return create_config_guide(root, &destination);
        }
        Err(_) => return ConfigurationGuideStatus::Unavailable,
    };
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        return ConfigurationGuideStatus::UserModified;
    }

    let existing = match fs::read(&destination) {
        Ok(contents) => contents,
        Err(_) => return ConfigurationGuideStatus::Unavailable,
    };
    let classification = classify_runtime_config_guide(&existing);
    match classification {
        ConfigGuideClassification::Current => ConfigurationGuideStatus::Current,
        ConfigGuideClassification::Future(revision) => {
            ConfigurationGuideStatus::FutureRevision { revision }
        }
        ConfigGuideClassification::Modified => ConfigurationGuideStatus::UserModified,
    }
}

/// 运行时仅认当前正文和经过摘要校验的未来修订，不包含历史版本升级规则。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigGuideClassification {
    Current,
    Future(u32),
    Modified,
}

/// 只将当前修订识别为受管理指南；旧摘要白名单留在一次性迁移模块。
///
/// @author kongweiguang
fn classify_runtime_config_guide(contents: &[u8]) -> ConfigGuideClassification {
    let Some((revision, digest)) = config_guide_marker(contents) else {
        return ConfigGuideClassification::Modified;
    };
    if revision > CONFIG_GUIDE_REVISION {
        return ConfigGuideClassification::Future(revision);
    }
    if revision == CONFIG_GUIDE_REVISION && digest == config_guide_digest(CONFIG_GUIDE) {
        ConfigGuideClassification::Current
    } else {
        ConfigGuideClassification::Modified
    }
}

/// 解析生成标记并校验正文摘要，避免将手工复制的标记误当作生成文件所有权证明。
///
/// @author kongweiguang
pub(crate) fn config_guide_marker(contents: &[u8]) -> Option<(u32, String)> {
    let source = std::str::from_utf8(contents).ok()?;
    let (header, body) = source.split_once('\n')?;
    let header = header.strip_suffix('\r').unwrap_or(header);
    let metadata = header.strip_prefix(CONFIG_GUIDE_MARKER_PREFIX)?;
    let metadata = metadata.strip_suffix(" -->")?;
    let (revision, expected_digest) = metadata.split_once(" sha256=")?;
    let revision = revision.parse::<u32>().ok()?;
    let digest = config_guide_digest(body);
    (digest == expected_digest).then_some((revision, digest))
}

/// 生成同源文档的机器标记；摘要规范化换行符，兼容 Windows 与 Unix 用户目录。
///
/// @author kongweiguang
pub(crate) fn managed_config_guide() -> Vec<u8> {
    let digest = config_guide_digest(CONFIG_GUIDE);
    format!(
        "{CONFIG_GUIDE_MARKER_PREFIX}{CONFIG_GUIDE_REVISION} sha256={digest} -->\n{CONFIG_GUIDE}"
    )
    .into_bytes()
}

/// 按 UTF-8 文本的 LF 规范形计算摘要，避免编辑器换行转换被误判成用户修改。
///
/// @author kongweiguang
pub(crate) fn config_guide_digest(source: &str) -> String {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let digest = Sha256::digest(normalized.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 通过同目录 staging + CreateNew 播种指南，竞争创建时不覆盖先到的用户文件。
fn create_config_guide(root: &Path, destination: &Path) -> ConfigurationGuideStatus {
    let content = managed_config_guide();
    match commit_config_guide(root, destination, &content, CommitMode::CreateNew) {
        Ok(()) => ConfigurationGuideStatus::Created,
        Err(AtomicCommitError::NotCommitted(error))
            if error.kind() == io::ErrorKind::AlreadyExists =>
        {
            ConfigurationGuideStatus::UserModified
        }
        Err(AtomicCommitError::NotCommitted(_) | AtomicCommitError::CommitUnknown(_)) => {
            ConfigurationGuideStatus::Unavailable
        }
    }
}

/// 完整写入并同步同目录 staging 文件，再由统一平台原语实施 no-clobber 或原子替换。
fn commit_config_guide(
    root: &Path,
    destination: &Path,
    content: &[u8],
    mode: CommitMode,
) -> Result<(), AtomicCommitError> {
    let staging = root.join(format!(".kerminal-config-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(AtomicCommitError::NotCommitted)?;
        file.write_all(content)
            .map_err(AtomicCommitError::NotCommitted)?;
        file.sync_all().map_err(AtomicCommitError::NotCommitted)?;
        drop(file);
        commit_staged_file(&staging, destination, mode)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

/// 将受限文档 ID 转成固定配置相对路径，并拒绝任何逃逸组件。
fn relative_path(path: &ConfigDocumentPath) -> AppResult<PathBuf> {
    let relative = path
        .relative_path()
        .map_err(|_| AppError::invalid("配置文档路径无效"))?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(AppError::invalid("配置文档路径无效"));
    }
    Ok(relative)
}

/// 将领域枚举路径安全地连接到唯一数据根，防止未来路径类型扩展引入目录逃逸。
fn safe_join(root: &Path, relative: &Path) -> AppResult<PathBuf> {
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(AppError::invalid("配置文档路径无效"));
    }
    Ok(root.join(relative))
}

/// 只读检查当前配置格式，供一次性迁移预检复用且不初始化指南或恢复事务。
///
/// @author kongweiguang
pub(crate) fn inspect_current_configuration(root: &Path) -> ConfigurationRecoveryState {
    validation::inspect_startup_configuration(root)
}

/// 对 planner 生成的单个文档复用同一 schema validator，避免迁移路径复制字段规则。
///
/// @author kongweiguang
pub(crate) fn validate_current_configuration_document(
    path: &ConfigDocumentPath,
    source: &str,
) -> AppResult<()> {
    validation::validate_document(path, source)
        .map_err(|_| AppError::invalid("配置迁移结果不符合当前文档格式"))
}

/// 读取配置文件时遵循旧工作区的链接语义；破损链接仍作为不可读错误暴露。
fn read_configuration_file(path: &Path) -> io::Result<Option<String>> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = if link_metadata.file_type().is_symlink() {
        fs::metadata(path)?
    } else {
        link_metadata
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "configuration target must be a regular file",
        ));
    }
    let bytes = fs::read(path)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))
}

/// 以内容字节计算 revision，避免 TOML 重新序列化丢失注释或未知字段后误判冲突。
fn revision_for(source: &str) -> ConfigRevision {
    let digest = Sha256::digest(source.as_bytes());
    let hex = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    ConfigRevision::from_sha256(hex)
}

/// 把扫描失败压缩成不带绝对路径的相对恢复项。
fn unreadable_issue(relative_path: String) -> ConfigurationIssue {
    ConfigurationIssue {
        relative_path,
        kind: ConfigurationIssueKind::Unreadable,
        line: None,
        column: None,
    }
}

/// 把用户文件名中的控制字符转义，避免恢复状态直接产生多行或终端控制序列。
fn safe_file_name(name: &OsStr) -> String {
    name.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_control() {
                format!("\\u{{{:x}}}", character as u32)
            } else {
                character.to_string()
            }
        })
        .collect()
}

/// 将合法扫描相对路径统一成正斜杠显示格式，并转义控制字符。
fn safe_relative_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(safe_file_name(part)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 返回不依赖系统时钟格式的毫秒时间戳，保持 profile 字段兼容现有字符串时间。
fn unix_millis_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

/// 同目录 staging 后二次验证 revision，再以 no-clobber 或替换原语提交完整文件。
fn atomic_replace(
    root: &Path,
    relative: &Path,
    contents: &[u8],
    before_commit: impl FnOnce() -> AppResult<CommitMode>,
) -> AppResult<()> {
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("配置文档路径无效"))?;
    fs::create_dir_all(parent)
        .map_err(|source| AppError::infrastructure_with_source("创建配置目录失败", source))?;

    let mut temp_name = path
        .file_name()
        .ok_or_else(|| AppError::invalid("配置文档路径无效"))?
        .to_os_string();
    temp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temp_path = parent.join(temp_name);
    let write_result = (|| -> AppResult<()> {
        #[cfg(unix)]
        let mut temp_file = {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp_path)
        }
        .map_err(|source| AppError::infrastructure_with_source("创建配置暂存文件失败", source))?;
        #[cfg(not(unix))]
        let mut temp_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| {
                AppError::infrastructure_with_source("创建配置暂存文件失败", source)
            })?;

        use std::io::Write;
        temp_file.write_all(contents).map_err(|source| {
            AppError::infrastructure_with_source("写入配置暂存文件失败", source)
        })?;
        temp_file.sync_all().map_err(|source| {
            AppError::infrastructure_with_source("同步配置暂存文件失败", source)
        })?;
        drop(temp_file);
        let mode = before_commit()?;
        commit_staged_file(&temp_path, &path, mode).map_err(|error| match error {
            AtomicCommitError::NotCommitted(source) => {
                if final_path_is_symlink(&path).unwrap_or(false) {
                    AppError::conflict("符号链接配置可以读取，但不能通过编辑器覆盖")
                        .with_source(source)
                } else if matches!(
                    source.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) {
                    AppError::conflict("配置文件状态已变化，请重新读取后保存").with_source(source)
                } else {
                    AppError::infrastructure_with_source("原子保存配置文档失败", source)
                }
            }
            AtomicCommitError::CommitUnknown(source) => {
                AppError::outcome_unknown_with_source("配置保存结果未知，请重新读取后确认", source)
            }
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

/// 仅识别最终文件名上的链接，父目录链接仍按当前工作区的路径用法正常读取和写入。
fn final_path_is_symlink(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
struct ConfigurationWriteBlocked;

impl std::fmt::Display for ConfigurationWriteBlocked {
    /// 只表示启动门禁拒绝写入，不带损坏文件路径或内容。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("configuration writes are disabled during recovery")
    }
}

impl Error for ConfigurationWriteBlocked {}

/// Profile 文件的磁盘结构保持 schema 1 原字段并允许未知字段被读取后原文保留。
#[derive(Deserialize)]
struct ProfileTomlDocument {
    id: String,
    name: String,
    shell: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    is_default: bool,
    #[serde(default)]
    sidebar_group_id: Option<String>,
    sort_order: i64,
    created_at: String,
    updated_at: String,
}

/// Profile 表单写回使用的已知 schema 字段；Serialize 结果仅用于合并原 TOML 的对应键。
#[derive(Serialize)]
struct ProfileTomlWrite {
    schema_version: i64,
    id: String,
    name: String,
    shell: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    env: BTreeMap<String, String>,
    is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    sidebar_group_id: Option<String>,
    sort_order: i64,
    created_at: String,
    updated_at: String,
}

impl ProfileTomlWrite {
    /// 将已提交的表单内容转成纯领域 profile，schema 版本仍只属于文件层。
    fn into_profile(self) -> TerminalProfile {
        TerminalProfile {
            id: self.id,
            name: self.name,
            shell: self.shell,
            args: self.args,
            cwd: self.cwd,
            env: self.env,
            is_default: self.is_default,
            sidebar_group_id: self.sidebar_group_id,
            sort_order: self.sort_order,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

impl ProfileTomlDocument {
    /// 转换成默认终端目标读取所需的纯 profile 值，不重新序列化原 TOML。
    fn into_profile(self) -> TerminalProfile {
        TerminalProfile {
            id: self.id,
            name: self.name,
            shell: self.shell,
            args: self.args,
            cwd: self.cwd,
            env: self.env,
            is_default: self.is_default,
            sidebar_group_id: self.sidebar_group_id,
            sort_order: self.sort_order,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// 保留文件 ID 不一致的机器可诊断原因，同时不给公开错误附带原始内容。
#[derive(Debug)]
struct ProfilePathMismatch;

impl std::fmt::Display for ProfilePathMismatch {
    /// 返回固定摘要，避免在错误文本中拼接由文件内容提供的 ID。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("profile id does not match its file name")
    }
}

impl Error for ProfilePathMismatch {}
