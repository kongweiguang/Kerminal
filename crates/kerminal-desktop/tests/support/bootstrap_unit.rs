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
                for formal_root in [false, true] {
                    assert_eq!(
                        legacy_window_state_enabled(
                            development,
                            explicit_root,
                            environment_override,
                            formal_root
                        ),
                        formal_root && !development && !explicit_root && !environment_override,
                    );
                }
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
    let (_, host_types) = crate::plugin_catalogs::builtin_catalogs().expect("冻结内置目录");
    let service = migration_service(&root, None, host_types);
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

/// 已安装提供方的 codec 必须进入迁移预检；缺失提供方时保留同一份原文。
///
/// @author kongweiguang
#[test]
fn migration_service_uses_the_supplied_host_catalog() {
    use kerminal_application::connections::host_types::HostConfigurationType;
    use kerminal_infrastructure::configuration::host_configuration_catalog;

    let fixture = tempfile::tempdir().expect("隔离配置根");
    let hosts = fixture.path().join("hosts");
    std::fs::create_dir(&hosts).expect("创建主机目录");
    let path = hosts.join("migration-host.toml");
    let raw = "schema_version = 3\nid = \"migration-host\"\nprotocol = \"example-host\"\nprovider_id = \"example.plugin\"\nname = \"Migration\"\nsort_order = 0\nconfiguration_version = 1\ncreated_at = \"2026-01-01T00:00:00Z\"\nupdated_at = \"2026-01-01T00:00:00Z\"\n\n[configuration]\naddress = \"\"\n";
    std::fs::write(&path, raw).expect("写入插件主机");
    let unknown = host_configuration_catalog(Vec::new()).expect("内置目录");
    let codec = unknown
        .get("kerminal", "ssh")
        .expect("内置 codec")
        .codec
        .clone();
    let extension =
        HostConfigurationType::new("example.plugin", "example-host", codec).expect("扩展类型");
    let known = host_configuration_catalog(vec![extension]).expect("扩展目录");

    assert_eq!(
        migration_service(fixture.path(), None, known)
            .inspect()
            .expect("已安装预检")
            .readiness,
        DataMigrationReadiness::Blocked
    );
    assert_eq!(
        migration_service(fixture.path(), None, unknown)
            .inspect()
            .expect("缺失提供方预检")
            .readiness,
        DataMigrationReadiness::Legacy
    );
    assert_eq!(std::fs::read_to_string(path).expect("原文未改写"), raw);
}
