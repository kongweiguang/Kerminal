//! 独立编译存储模块的故障注入测试，避免启动桌面或运行整套基础设施单测。
//! @author kongweiguang

// 直接编译共享锁与原子提交实现，使恢复测试覆盖真实的私有删除和目录同步路径。
#[allow(dead_code)]
#[path = "../src/storage.rs"]
pub mod storage;

#[allow(dead_code)]
#[path = "../src/plugins/store.rs"]
mod plugin_store_under_test;

// 与存储模块一同编译最新启动入口，确认恢复门禁确实传播到启动和阶段推进。
use plugin_store_under_test as store;

#[allow(dead_code)]
#[path = "../src/plugins/boot.rs"]
mod boot_under_test;

struct NoPackages;

impl boot_under_test::BootPackageValidator for NoPackages {
    type Manifest = ();

    /// 本场景没有插件目录；若意外进入校验应立即失败，避免掩盖启动路径变化。
    fn validate(
        &self,
        _directory: &std::path::Path,
        _id: &kerminal_domain::plugins::PluginId,
        _version: &kerminal_domain::plugins::PluginVersion,
        _source: kerminal_domain::plugins::PluginSource,
    ) -> Result<(), boot_under_test::BootPackageError> {
        panic!("no packages should be validated")
    }
}

#[test]
/// 同一份待恢复记录阻止启动事务和已有启动身份推进，避免旁路状态门禁。
fn recovery_blocks_boot_start_and_phase_updates() {
    use kerminal_domain::plugins::PluginBootPhase;
    use store::{PluginStore, PluginStoreError};

    let root = tempfile::tempdir().unwrap();
    let store = PluginStore::new(storage::StorageCoordinator::new(root.path())).unwrap();
    let boot = store.begin_startup(&NoPackages, false).unwrap();
    let mut candidate = boot.store.state.clone();
    candidate.revision += 1;
    let path = root.path().join("plugins/state-recovery-legacy.json");
    std::fs::write(path, serde_json::to_vec_pretty(&candidate).unwrap()).unwrap();
    assert!(matches!(
        store.begin_startup(&NoPackages, false),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
    assert!(matches!(
        store.mark_boot_phase(&boot.boot_id, PluginBootPhase::RegistryFrozen),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
}
