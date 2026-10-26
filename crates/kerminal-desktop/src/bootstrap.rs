//! 桌面与无窗口维护入口共享的迁移门禁，不装配任何运行态业务资源。
//!
//! @author kongweiguang

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use kerminal_application::{
    AppResult,
    data_migration::{DataMigrationReport, DataMigrationService},
};
use kerminal_infrastructure::data_migration::FileDataMigrationRepository;

use crate::config_root::ConfigRoot;

/// 两入口构造同一用例；旧窗口读取权限与旧实例检查分开，显式根指向正式根仍须保护旧版写入者。
///
/// @author kongweiguang
pub(crate) fn migration_service(
    root: &Path,
    legacy_window_state: Option<&Path>,
) -> DataMigrationService {
    let repository = FileDataMigrationRepository::new(root);
    let repository = match legacy_window_state {
        Some(path) => repository.with_legacy_window_state(path),
        None => repository,
    };
    let repository = if ConfigRoot::points_to_formal_default(root) {
        repository.with_legacy_instance_check()
    } else {
        repository
    };
    DataMigrationService::new(Arc::new(repository))
}

/// 迁移成功后才调用后继动作，形成可验证的装配屏障；未知或失败结果不得创建任何运行态资源。
///
/// @author kongweiguang
pub(crate) fn run_after_migration<T>(
    service: &DataMigrationService,
    start: impl FnOnce(DataMigrationReport) -> T,
) -> AppResult<T> {
    let report = service.migrate()?;
    Ok(start(report))
}

/// GPUI 默认测试根不读取旧 Tauri 窗口资料，避免旧版仍在运行时共享其状态文件。
/// 正式根的原有来源限制保留，显式 CLI 根和环境隔离根也不查询该来源。
///
/// @author kongweiguang
pub(crate) fn legacy_window_state_path(explicit_root: bool) -> Option<PathBuf> {
    if !legacy_window_state_enabled(
        cfg!(debug_assertions),
        explicit_root,
        std::env::var_os("KERMINAL_CONFIG_ROOT").is_some(),
        ConfigRoot::resolve()
            .ok()
            .is_some_and(|root| ConfigRoot::points_to_formal_default(root.as_path())),
    ) {
        return None;
    }
    dirs::config_dir().map(|directory| {
        directory
            .join("io.github.kongweiguang.kerminal")
            .join(".window-state.json")
    })
}

/// 来源权限同时检查目录身份和调用方式，使 release 的 GPUI 测试根也保持隔离。
///
/// @author kongweiguang
fn legacy_window_state_enabled(
    development: bool,
    explicit_root: bool,
    environment_override: bool,
    formal_root: bool,
) -> bool {
    formal_root && !development && !explicit_root && !environment_override
}

#[cfg(test)]
#[path = "../tests/support/bootstrap_unit.rs"]
mod tests;
