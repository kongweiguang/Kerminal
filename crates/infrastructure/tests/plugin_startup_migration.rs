//! 首次插件启动与格式迁移按生产顺序共存，未知同名文件不能绕过数据根门禁。
//! @author kongweiguang

use kerminal_application::data_migration::{DataMigrationReadiness, DataMigrationRepository};
use kerminal_infrastructure::{
    data_migration::FileDataMigrationRepository,
    plugins::{boot::ProtocolBootValidator, store::PluginStore},
    storage::StorageCoordinator,
};
use kerminal_plugin_protocol::Platform;

/// 启动快照在迁移前创建状态文件；迁移后应保留原字节，随后重复启动仍能使用该状态。
#[test]
fn first_plugin_boot_does_not_block_fresh_root_migration() {
    let root = tempfile::tempdir().expect("隔离数据根");
    let store = PluginStore::new(StorageCoordinator::new(root.path())).expect("插件存储");
    let validator = ProtocolBootValidator {
        host_api: "1.0.0".into(),
        platform: Platform::current().expect("受支持的测试平台"),
    };
    let boot = store
        .begin_startup(&validator, false)
        .expect("首次启动快照");
    store
        .mark_boot_phase(
            &boot.boot_id,
            kerminal_domain::plugins::PluginBootPhase::RegistryFrozen,
        )
        .expect("冻结目录");
    let state_path = root.path().join("plugins/state.json");
    let before = std::fs::read(&state_path).expect("插件状态原文");
    let migration = FileDataMigrationRepository::new(root.path());
    assert_ne!(
        migration.inspect().expect("只读预检").readiness,
        DataMigrationReadiness::Blocked
    );
    migration.migrate().expect("正常初始化格式标记");
    assert_eq!(
        migration.inspect().expect("迁移后预检").readiness,
        DataMigrationReadiness::Current
    );
    assert_eq!(
        std::fs::read(&state_path).expect("插件状态未被改写"),
        before
    );
    assert!(store.begin_startup(&validator, false).is_ok());
}

/// 伪造、未来格式或不合法状态不能仅凭路径被识别为可自动迁移的用户数据根。
#[test]
fn unrecognized_plugin_state_does_not_identify_a_managed_root() {
    for bytes in [
        b"{}".as_slice(),
        br#"{"schema":1,"revision":0,"plugins":{},"pending":[],"failures":{},"boot":{"id":"invalid","phase":"starting","consecutive_early_crashes":0}}"#,
        // 未来格式也不能被认领；最小合法 schema 2 引导状态在上一用例中单独验证。
        br#"{"schema":3,"revision":0,"plugins":{},"pending":[],"failures":{}}"#,
    ] {
        let root = tempfile::tempdir().expect("隔离数据根");
        std::fs::create_dir(root.path().join("plugins")).expect("插件目录");
        let path = root.path().join("plugins/state.json");
        std::fs::write(&path, bytes).expect("不可信状态");
        let migration = FileDataMigrationRepository::new(root.path());
        assert_eq!(
            migration.inspect().expect("只读预检").readiness,
            DataMigrationReadiness::Blocked
        );
        assert!(migration.migrate().is_err());
        assert_eq!(std::fs::read(path).expect("原文保留"), bytes);
    }
}

/// 插件启动写出的合法状态不能使包含用户资料的未知根获得自动迁移资格。
#[test]
fn generated_plugin_state_cannot_bypass_an_unknown_nonempty_root() {
    for relative in ["notes.txt", "plugins/notes.txt"] {
        let root = tempfile::tempdir().expect("隔离数据根");
        let note = root.path().join(relative);
        std::fs::create_dir_all(note.parent().expect("父目录")).expect("资料目录");
        std::fs::write(&note, "保留用户原文").expect("用户资料");
        let store = PluginStore::new(StorageCoordinator::new(root.path())).expect("插件存储");
        store
            .begin_startup(
                &ProtocolBootValidator {
                    host_api: "1.0.0".into(),
                    platform: Platform::current().expect("测试平台"),
                },
                false,
            )
            .expect("插件引导写出有效状态");
        let migration = FileDataMigrationRepository::new(root.path());
        assert_eq!(
            migration.inspect().expect("只读预检").readiness,
            DataMigrationReadiness::Blocked
        );
        assert!(migration.migrate().is_err());
        assert_eq!(
            std::fs::read_to_string(note).expect("用户资料仍在"),
            "保留用户原文"
        );
        assert!(!root.path().join(".kerminal-format.toml").exists());
    }
}
