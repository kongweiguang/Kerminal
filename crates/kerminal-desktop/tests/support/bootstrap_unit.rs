//! 迁移门禁与旧来源权限回归；不装配 runtime、日志、单实例或窗口。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use kerminal_application::{
    AppError, AppResult,
    data_migration::{
        DataMigrationInspection, DataMigrationReadiness, DataMigrationReport,
        DataMigrationRepository, DataMigrationService,
    },
};

use super::{legacy_window_state_enabled, migration_service, run_after_migration};

struct RejectingMigration {
    called: Arc<AtomicBool>,
    error: AppError,
}

impl DataMigrationRepository for RejectingMigration {
    /// 启动门禁必须调用 migrate，不能只检查后继续装配；不应调用的端口直接使测试失败。
    ///
    /// @author kongweiguang
    fn inspect(&self) -> AppResult<DataMigrationInspection> {
        panic!("startup must complete migration rather than only inspect");
    }

    /// 精确保留仓储的确定失败、冲突或未知分类，让启动层只承担失败时停止装配的责任。
    ///
    /// @author kongweiguang
    fn migrate(&self) -> AppResult<DataMigrationReport> {
        self.called.store(true, Ordering::SeqCst);
        Err(self.error.clone())
    }
}

/// 任一隔离条件都禁止查询旧 Tauri 来源，不需要修改进程环境或接触真实用户目录来测试。
///
/// @author kongweiguang
#[test]
fn legacy_source_is_only_enabled_for_the_formal_default_root() {
    for development in [false, true] {
        for explicit_root in [false, true] {
            for environment_override in [false, true] {
                assert_eq!(
                    legacy_window_state_enabled(development, explicit_root, environment_override),
                    !development && !explicit_root && !environment_override,
                );
            }
        }
    }
}

/// 损坏、未来格式、锁冲突和未知提交都必须阻断后继资源构造，且不能把原始分类变成成功。
///
/// @author kongweiguang
#[test]
fn failed_or_unknown_migration_never_starts_desktop_resources() {
    for error in [
        AppError::configuration_corrupt("迁移测试"),
        AppError::unsupported("未来数据格式"),
        AppError::conflict("迁移锁正忙"),
        AppError::outcome_unknown("迁移提交待确认"),
    ] {
        let called = Arc::new(AtomicBool::new(false));
        let started = AtomicBool::new(false);
        let expected_code = error.public_code();
        let expected_message = error.public_message();
        let service = DataMigrationService::new(Arc::new(RejectingMigration {
            called: Arc::clone(&called),
            error,
        }));
        let result = run_after_migration(&service, |_| started.store(true, Ordering::SeqCst));
        let error = result.expect_err("migration failure must stop startup");
        assert!(called.load(Ordering::SeqCst));
        assert!(!started.load(Ordering::SeqCst));
        assert_eq!(error.public_code(), expected_code);
        assert_eq!(error.public_message(), expected_message);
    }
}

/// 成功屏障必须先把真实临时根提交为 Current，再允许调用方进入普通桌面装配。
///
/// @author kongweiguang
#[test]
fn successful_migration_commits_before_the_start_callback() {
    let fixture = tempfile::tempdir().expect("isolated bootstrap fixture");
    let root = fixture.path().join("new-current-root");
    let service = migration_service(&root, None);
    let started = AtomicBool::new(false);
    run_after_migration(&service, |_| {
        assert!(matches!(
            service
                .inspect()
                .expect("committed migration inspection")
                .readiness,
            DataMigrationReadiness::Current,
        ));
        started.store(true, Ordering::SeqCst);
    })
    .expect("migration succeeded");
    assert!(started.load(Ordering::SeqCst));
}
