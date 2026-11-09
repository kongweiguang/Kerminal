//! 离线插件快照不得触发启动状态或待生效操作。
//! @author kongweiguang

use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginId, PluginSource, PluginVersion,
};
use kerminal_infrastructure::{
    plugins::{
        boot::{BootPackageError, BootPackageValidator},
        store::PluginStore,
    },
    storage::StorageCoordinator,
};
use std::{fs, path::Path};

struct Validator;
impl BootPackageValidator for Validator {
    type Manifest = ();
    /// 只接受存在的夹具清单，确保目录自身不代表包可用。
    fn validate(
        &self,
        directory: &Path,
        _: &PluginId,
        _: &PluginVersion,
        _: PluginSource,
    ) -> Result<(), BootPackageError> {
        fs::read(directory.join("manifest"))
            .map(|_| ())
            .map_err(|_| BootPackageError::Corrupt)
    }
}

/// 离线读取保留禁用插件声明和待处理操作，并且不触碰状态文件。
#[test]
fn snapshot_is_read_only_and_includes_disabled_packages() {
    let temp = tempfile::tempdir().unwrap();
    let store = PluginStore::new(StorageCoordinator::new(temp.path())).unwrap();
    let id = PluginId::parse("acme.example").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    let package = temp.path().join("plugins/packages/acme.example/1.0.0");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("manifest"), b"fixture").unwrap();
    let before = store.load().unwrap();
    let mut state = before.state.clone();
    state.plugins.insert(
        id.clone(),
        InstalledPlugin {
            active: version.clone(),
            previous: None,
            enabled: false,
            source: PluginSource::Local,
            installed_at: "2026-11-09T00:00:00Z".into(),
        },
    );
    state
        .pending
        .push(PendingPluginChange::Enable { id: id.clone() });
    store.compare_and_swap(&before, state).unwrap();
    let path = temp.path().join("plugins/state.json");
    let bytes = fs::read(&path).unwrap();
    let (plugins, rejected) = store.read_static_plugins(&Validator).unwrap();
    assert!(rejected.is_empty());
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].id, id);
    assert!(!plugins[0].enabled);
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(!temp.path().join(".storage.lock").exists());
}

/// 不存在的配置根保持不存在，离线查询不能初始化用户数据目录。
#[test]
fn missing_root_is_not_created() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing");
    let store = PluginStore::new(StorageCoordinator::new(&root)).unwrap();
    assert!(store.read_static_plugins(&Validator).unwrap().0.is_empty());
    assert!(!root.exists());
}

struct ConcurrentValidator(PluginStore);
impl BootPackageValidator for ConcurrentValidator {
    type Manifest = ();
    /// 模拟另一个进程在包验证期间提交状态，检查快照不能混用两个版本。
    fn validate(
        &self,
        _: &Path,
        _: &PluginId,
        _: &PluginVersion,
        _: PluginSource,
    ) -> Result<(), BootPackageError> {
        let current = self.0.load().map_err(|_| BootPackageError::Unavailable)?;
        self.0
            .compare_and_swap(&current, current.state.clone())
            .map_err(|_| BootPackageError::Unavailable)?;
        Ok(())
    }
}

/// 读取期间状态变化时拒绝结果，调用方必须重新建立一致快照。
#[test]
fn changed_state_rejects_inconsistent_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let store = PluginStore::new(StorageCoordinator::new(temp.path())).unwrap();
    let id = PluginId::parse("acme.example").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    fs::create_dir_all(temp.path().join("plugins/packages/acme.example/1.0.0")).unwrap();
    let before = store.load().unwrap();
    let mut state = before.state.clone();
    state.plugins.insert(
        id,
        InstalledPlugin {
            active: version,
            previous: None,
            enabled: true,
            source: PluginSource::Local,
            installed_at: "2026-11-09T00:00:00Z".into(),
        },
    );
    store.compare_and_swap(&before, state).unwrap();
    let other = PluginStore::new(StorageCoordinator::new(temp.path())).unwrap();
    assert!(matches!(
        store.read_static_plugins(&ConcurrentValidator(other)),
        Err(kerminal_infrastructure::plugins::store::PluginStoreError::Conflict)
    ));
}
