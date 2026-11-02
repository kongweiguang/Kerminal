//! 将旧格式判定、规划和磁盘提交收口到同一迁移 adapter。
//!
//! @author kongweiguang

mod agents;
mod configuration;
mod inspection;
mod journal;
mod marker;
mod offline_guard;
mod operational_state;
mod profile_hosts;
mod profile_import;
mod sqlite;
mod transaction;
mod vault;
mod window_state;
mod workspace;

use std::{
    fmt,
    path::{Path, PathBuf},
};

use kerminal_application::{
    AppError, AppResult,
    data_migration::{
        DataMigrationInspection, DataMigrationOutcome, DataMigrationReadiness, DataMigrationReport,
        DataMigrationRepository, ProfileImportPreview,
    },
};

use crate::storage::StorageCoordinator;
use zeroize::Zeroize;

/// 业务 planner 对单个固定相对路径提出的 CAS 内容变更。
///
/// `None` 表示该文件按预期缺失；实际提交前 core 仍会重新读取并核对。
#[derive(Clone, PartialEq, Eq)]
pub(super) struct FileEdit {
    pub relative_path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
}

impl fmt::Debug for FileEdit {
    /// 迁移项可能包含旧环境变量或新 Vault 密钥；诊断只展示路径与字节数量。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileEdit")
            .field("relative_path", &self.relative_path)
            .field("before_bytes", &self.before.as_ref().map(Vec::len))
            .field("after_bytes", &self.after.len())
            .finish()
    }
}

impl Drop for FileEdit {
    /// dry-run 和失败路径同样清除计划中的密钥与用户配置字节，避免普通 Vec 长期残留。
    fn drop(&mut self) {
        if let Some(before) = &mut self.before {
            before.zeroize();
        }
        self.after.zeroize();
    }
}

/// 适配桌面与 console 的显式迁移存储入口；构造本身不触碰数据根。
#[derive(Clone)]
pub struct FileDataMigrationRepository {
    root: PathBuf,
    storage: StorageCoordinator,
    legacy_window_state: Option<PathBuf>,
    legacy_instance_check: bool,
}

impl FileDataMigrationRepository {
    /// 绑定调用方解析好的数据根；延迟创建锁与目录，确保 inspect 保持纯只读。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            storage: StorageCoordinator::new(root.clone()),
            root,
            legacy_window_state: None,
            legacy_instance_check: false,
        }
    }

    /// 只为正式默认根显式提供旧 Tauri 窗口状态位置；隔离根默认不会访问该位置。
    pub fn with_legacy_window_state(mut self, path: impl Into<PathBuf>) -> Self {
        self.legacy_window_state = Some(path.into());
        self
    }

    /// 启用旧版单实例探测；该探测不是跨平台租约，调用方还须遵守关闭旧版的离线约束。
    pub fn with_legacy_instance_check(mut self) -> Self {
        self.legacy_instance_check = true;
        self
    }
}

impl DataMigrationRepository for FileDataMigrationRepository {
    /// 只使用只读 scanner；不会创建根目录、共享锁、事务文件或 SQLite 辅助文件。
    fn inspect(&self) -> AppResult<DataMigrationInspection> {
        inspection::inspect_root(&self.root, self.legacy_window_state.as_deref())
    }

    /// apply 先取得共享根锁并重扫；current 根只允许按冻结正文更新指南，不进入业务旧格式 planner。
    ///
    /// @author kongweiguang
    fn migrate(&self) -> AppResult<DataMigrationReport> {
        if let Some(fast) = inspection::inspect_current_fast_path(&self.root)? {
            if fast.readiness == DataMigrationReadiness::Current
                && !fast.recovery_pending
                && let Some(report) = self.migrate_current_guide_only()?
            {
                return Ok(report);
            }
            if fast.readiness == DataMigrationReadiness::Blocked {
                return Err(AppError::conflict("用户数据迁移预检存在阻断项"));
            }
        }
        let initial = inspection::inspect_root(&self.root, self.legacy_window_state.as_deref())?;
        if initial.readiness == DataMigrationReadiness::Current
            && !initial.recovery_pending
            && let Some(report) = self.migrate_current_guide_only()?
        {
            return Ok(report);
        }
        if initial.readiness == DataMigrationReadiness::Blocked {
            return Err(AppError::conflict("用户数据迁移预检存在阻断项"));
        }
        let _storage_guard = self
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("用户数据迁移存储锁不可用"))?;
        let inspection = inspection::inspect_root(&self.root, self.legacy_window_state.as_deref())?;
        match inspection.readiness {
            DataMigrationReadiness::Current if !inspection.recovery_pending => {
                Ok(already_current_report())
            }
            DataMigrationReadiness::Blocked => {
                Err(AppError::conflict("用户数据迁移预检存在阻断项"))
            }
            DataMigrationReadiness::Fresh
            | DataMigrationReadiness::Legacy
            | DataMigrationReadiness::Current => {
                if self.legacy_instance_check
                    && inspection.readiness != DataMigrationReadiness::Fresh
                {
                    offline_guard::ensure_legacy_instance_stopped()?;
                }
                transaction::apply_locked(&self.root, &self.legacy_window_state, inspection)
            }
        }
    }

    /// 预演仅读取两个指定根并复用实际规划，随机密钥仅在内存中生成并随计划清除。
    fn preview_legacy_profiles(&self, source_root: &Path) -> AppResult<ProfileImportPreview> {
        let source = self.validate_import_roots(source_root)?;
        let plan = profile_import::plan_profile_import(&source, &self.root)?;
        inspection::validate_edits(&self.root, &plan.edits)
            .map_err(|_| AppError::conflict("导入文件路径或源状态无效"))?;
        Ok(plan.preview)
    }

    /// 同时持有来源和目标的协作锁，按目标当前状态重新规划并通过单一事务提交。
    fn import_legacy_profiles(&self, source_root: &Path) -> AppResult<DataMigrationReport> {
        let source = self.validate_import_roots(source_root)?;
        let source_storage = StorageCoordinator::new(source.clone());
        let _source_guard = source_storage
            .lock()
            .map_err(|_| AppError::conflict("导入来源正在使用或无法取得存储锁"))?;
        let _target_guard = self
            .storage
            .lock()
            .map_err(|_| AppError::conflict("导入目标正在使用或无法取得存储锁"))?;
        let source = self.validate_import_roots(&source)?;
        let plan = profile_import::plan_profile_import(&source, &self.root)?;
        inspection::validate_edits(&self.root, &plan.edits)
            .map_err(|_| AppError::conflict("导入文件路径或源状态已变化"))?;
        if plan.edits.is_empty() {
            return Ok(already_current_report());
        }
        transaction::apply_current_profile_import_locked(&self.root, plan.edits)
    }
}

impl FileDataMigrationRepository {
    /// 仅允许旧来源与已完整迁移的目标；来源和目标解析为同一根时直接拒绝。
    fn validate_import_roots(&self, source_root: &Path) -> AppResult<PathBuf> {
        let source_metadata = std::fs::symlink_metadata(source_root)
            .map_err(|_| AppError::invalid("导入来源不存在或不可读取"))?;
        if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
            return Err(AppError::invalid("导入来源必须是普通目录"));
        }
        let source = std::fs::canonicalize(source_root)
            .map_err(|_| AppError::invalid("导入来源不存在或不可读取"))?;
        let target = std::fs::canonicalize(&self.root)
            .map_err(|_| AppError::invalid("导入目标不存在或不可读取"))?;
        if source.starts_with(&target) || target.starts_with(&source) {
            return Err(AppError::invalid("导入来源与目标不能相同或互相嵌套"));
        }
        match marker::read_marker(&source)? {
            marker::MarkerRead::Missing | marker::MarkerRead::PreviousSupported(_) => {}
            marker::MarkerRead::Current => {
                return Err(AppError::invalid("来源已是当前格式，请选择旧版数据根"));
            }
            marker::MarkerRead::Blocked(_) => {
                return Err(AppError::invalid("导入来源的数据格式不受支持"));
            }
        }
        let source_inspection = inspection::inspect_root(&source, None)?;
        if source_inspection.readiness == DataMigrationReadiness::Blocked
            || source_inspection.recovery_pending
        {
            return Err(AppError::conflict("导入来源需要先完成旧事务恢复"));
        }
        let current = inspection::inspect_current_fast_path(&self.root)?
            .ok_or_else(|| AppError::invalid("导入目标尚未完成当前格式初始化"))?;
        if current.readiness != DataMigrationReadiness::Current || current.recovery_pending {
            return Err(AppError::conflict("导入目标需要先完成现有迁移或恢复"));
        }
        if crate::configuration::inspect_current_configuration(&self.root).mode
            != kerminal_domain::preferences::ConfigurationMode::ReadWrite
        {
            return Err(AppError::conflict("导入目标配置需要先恢复"));
        }
        Ok(source)
    }

    /// Only frozen guide edits may use this current-contract branch; recheck readiness under the
    /// shared root lock so a stale read cannot overwrite user edits or bypass pending recovery.
    ///
    /// @author kongweiguang
    fn migrate_current_guide_only(&self) -> AppResult<Option<DataMigrationReport>> {
        if configuration::plan_current_config_guide_edit(&self.root)?.is_none() {
            return Ok(Some(already_current_report()));
        }

        let _storage_guard = self
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("用户数据迁移存储锁不可用"))?;
        let Some(current) = inspection::inspect_current_fast_path(&self.root)? else {
            return Ok(None);
        };
        if current.readiness == DataMigrationReadiness::Blocked {
            return Err(AppError::conflict("用户数据迁移预检存在阻断项"));
        }
        if current.readiness != DataMigrationReadiness::Current || current.recovery_pending {
            return Ok(None);
        }

        let Some(guide_edit) = configuration::plan_current_config_guide_edit(&self.root)? else {
            return Ok(Some(already_current_report()));
        };
        transaction::apply_current_guide_locked(&self.root, guide_edit).map(Some)
    }
}

/// 统一当前格式幂等回执，且不为只读路径创建根锁或本地文件。
fn already_current_report() -> DataMigrationReport {
    DataMigrationReport {
        outcome: DataMigrationOutcome::AlreadyCurrent,
        changed_file_count: 0,
        resumed: false,
        backup_relative_path: None,
        backup_relative_paths: Vec::new(),
    }
}
