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
mod sqlite;
mod transaction;
mod vault;
mod window_state;
mod workspace;

use std::path::PathBuf;

use kerminal_application::{
    AppError, AppResult,
    data_migration::{
        DataMigrationInspection, DataMigrationOutcome, DataMigrationReadiness, DataMigrationReport,
        DataMigrationRepository,
    },
};

use crate::storage::StorageCoordinator;

/// 业务 planner 对单个固定相对路径提出的 CAS 内容变更。
///
/// `None` 表示该文件按预期缺失；实际提交前 core 仍会重新读取并核对。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileEdit {
    pub relative_path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
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
}

impl FileDataMigrationRepository {
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
