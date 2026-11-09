//! 验证状态 CAS、启动隔离与不丢失用户数据的卸载顺序。
//! @author kongweiguang

use std::{fs, path::Path};

use kerminal_domain::plugins::{
    PendingPluginChange, PluginBootPhase, PluginId, PluginSource, PluginVersion,
};
use kerminal_infrastructure::{
    plugins::{
        boot::{BootPackageError, BootPackageValidator},
        store::{PluginStore, PluginStoreError},
    },
    storage::StorageCoordinator,
};

struct FixtureValidator;

impl BootPackageValidator for FixtureValidator {
    type Manifest = String;

    /// 测试只接受与目录身份完全一致的清单，避免把目录存在误认为版本完整。
    fn validate(
        &self,
        directory: &Path,
        id: &PluginId,
        version: &PluginVersion,
        _source: PluginSource,
    ) -> Result<String, BootPackageError> {
        let manifest = fs::read_to_string(directory.join("manifest"))
            .map_err(|_| BootPackageError::Corrupt)?;
        if manifest != format!("{id}@{version}") {
            return Err(BootPackageError::Corrupt);
        }
        Ok(manifest)
    }
}

/// 每个场景使用自己的配置根，不触碰用户真实插件状态。
fn store(root: &Path) -> PluginStore {
    PluginStore::new(StorageCoordinator::new(root)).unwrap()
}

/// 使用测试清单模拟已完整发布的不可变版本目录。
fn package(root: &Path, id: &PluginId, version: &PluginVersion) {
    let directory = root
        .join("plugins")
        .join("packages")
        .join(id.as_str())
        .join(version.to_string());
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("manifest"), format!("{id}@{version}")).unwrap();
}

/// 初次安装先写待处理意图，启动前不能看到活动插件。
fn install(store: &PluginStore, id: &PluginId, version: &PluginVersion) {
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Install {
        id: id.clone(),
        to: version.clone(),
        staged: format!("packages/{id}/{version}"),
        source: PluginSource::Local,
        installed_at: "2026-11-08T10:00:00Z".into(),
    });
    store.compare_and_swap(&current, next).unwrap();
}

#[test]
/// 并发草稿和损坏文件都不能被后写者覆盖；调用方必须显式读回。
fn cas_rejects_stale_and_corrupt_state_without_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let first = store.load().unwrap();
    let stale = first.clone();
    let committed = store.compare_and_swap(&first, first.state.clone()).unwrap();
    assert_eq!(committed.state.revision, 1);
    assert!(matches!(
        store.compare_and_swap(&stale, stale.state.clone()),
        Err(PluginStoreError::Conflict)
    ));
    let path = root.path().join("plugins/state.json");
    fs::write(&path, b"{broken").unwrap();
    assert!(matches!(store.load(), Err(PluginStoreError::InvalidState)));
    assert_eq!(fs::read(&path).unwrap(), b"{broken");
}

#[test]
/// 损坏更新保持旧版可用，同时保留待处理意图并隔离其他已安装插件。
fn failed_update_preserves_active_version_and_other_plugins() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let first = PluginId::parse("acme.notes").unwrap();
    let second = PluginId::parse("acme.tasks").unwrap();
    let old = PluginVersion::parse("1.0.0").unwrap();
    let new = PluginVersion::parse("1.1.0").unwrap();
    package(root.path(), &first, &old);
    package(root.path(), &second, &old);
    install(&store, &first, &old);
    install(&store, &second, &old);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.enabled_plugins().count(), 2);
    let mut next = boot.store.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: first.clone(),
        to: new.clone(),
        staged: format!("packages/{first}/{new}"),
    });
    store.compare_and_swap(&boot.store, next).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.store.state.plugins[&first].active, old);
    assert_eq!(boot.store.state.pending.len(), 1);
    assert_eq!(boot.rejected[&first], BootPackageError::Missing);
    assert_eq!(boot.enabled_plugins().count(), 2);
}

#[test]
/// 只有连续两次冻结后首帧前退出才触发安全模式，旧启动身份不能确认首帧。
fn safe_mode_counts_only_post_freeze_crashes() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let first = store.begin_startup(&FixtureValidator, false).unwrap();
    store
        .mark_boot_phase(&first.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();
    let second = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!second.safe_mode);
    assert!(matches!(
        store.mark_boot_phase(&first.boot_id, PluginBootPhase::FirstFrame),
        Err(PluginStoreError::Conflict)
    ));
    store
        .mark_boot_phase(&second.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();
    let third = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(third.safe_mode);
    store
        .mark_boot_phase(&third.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();
    store
        .mark_boot_phase(&third.boot_id, PluginBootPhase::FirstFrame)
        .unwrap();
    assert!(
        !store
            .begin_startup(&FixtureValidator, false)
            .unwrap()
            .safe_mode
    );
}

#[test]
/// 卸载只清理版本目录，独立数据目录必须保留。
fn uninstall_keeps_plugin_data() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    package(root.path(), &id, &version);
    let data = root.path().join("plugins/data").join(id.as_str());
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("notes"), b"keep").unwrap();
    install(&store, &id, &version);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let mut next = boot.store.state.clone();
    next.pending
        .push(PendingPluginChange::Uninstall { id: id.clone() });
    store.compare_and_swap(&boot.store, next).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!boot.store.state.plugins.contains_key(&id));
    assert!(
        !root
            .path()
            .join("plugins/packages")
            .join(id.as_str())
            .exists()
    );
    assert_eq!(fs::read(data.join("notes")).unwrap(), b"keep");
}

#[test]
/// 连续更新只保留活动和上一版本；未完成操作存在时暂缓清理该插件全部版本。
fn cleanup_keeps_active_previous_and_pending_versions() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let versions =
        ["1.0.0", "1.1.0", "1.2.0", "1.3.0"].map(|value| PluginVersion::parse(value).unwrap());
    package(root.path(), &id, &versions[0]);
    install(&store, &id, &versions[0]);
    let first = store.begin_startup(&FixtureValidator, false).unwrap();
    package(root.path(), &id, &versions[1]);
    let mut next = first.store.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: versions[1].clone(),
        staged: format!("packages/{id}/{}", versions[1]),
    });
    let queued = store.compare_and_swap(&first.store, next).unwrap();
    let second = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(second.store.state.revision > queued.state.revision);
    package(root.path(), &id, &versions[2]);
    let mut next = second.store.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: versions[2].clone(),
        staged: format!("packages/{id}/{}", versions[2]),
    });
    store.compare_and_swap(&second.store, next).unwrap();
    let third = store.begin_startup(&FixtureValidator, false).unwrap();
    let directory = root.path().join("plugins/packages").join(id.as_str());
    assert!(!directory.join(versions[0].to_string()).exists());
    assert!(directory.join(versions[1].to_string()).exists());
    assert!(directory.join(versions[2].to_string()).exists());
    package(root.path(), &id, &versions[0]);
    let mut next = third.store.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: versions[3].clone(),
        staged: format!("packages/{id}/{}", versions[3]),
    });
    store.compare_and_swap(&third.store, next).unwrap();
    let fourth = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(fourth.rejected[&id], BootPackageError::Missing);
    assert!(directory.join(versions[0].to_string()).exists());
}

#[test]
/// 未识别条目不删除且标记清理失败；障碍移除后下一次启动继续清理卸载残留。
fn cleanup_preserves_unknown_entries_and_retries_orphan_removal() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.orphan").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    package(root.path(), &id, &version);
    let directory = root.path().join("plugins/packages").join(id.as_str());
    fs::write(directory.join("unknown.txt"), b"keep").unwrap();
    let first = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(first.cleanup_failed);
    assert_eq!(fs::read(directory.join("unknown.txt")).unwrap(), b"keep");
    assert!(!directory.join(version.to_string()).exists());
    fs::remove_file(directory.join("unknown.txt")).unwrap();
    let second = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!second.cleanup_failed);
    assert!(!directory.exists());
}

#[test]
/// 启动后视图、翻译、schema 和网页资源来自内存快照，文件改变不会改变当前会话。
fn protocol_boot_freezes_views_translations_and_web_resources() {
    use kerminal_infrastructure::plugins::boot::ProtocolBootValidator;
    use kerminal_plugin_protocol::{MANIFEST_FILE_NAME, Platform};
    use serde_json::json;

    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    let directory = root
        .path()
        .join("plugins/packages")
        .join(id.as_str())
        .join(version.to_string());
    fs::create_dir_all(directory.join("views")).unwrap();
    fs::create_dir_all(directory.join("web")).unwrap();
    fs::create_dir_all(directory.join("l10n")).unwrap();
    let manifest = json!({
        "schema":1, "id":"acme.notes", "name":{"en":"Notes"}, "description":{"en":"Notes"},
        "version":"1.0.0", "api":"^1.0", "publisher":"acme", "runtime":{"kind":"none"},
        "l10n":"l10n", "views":{
            "native":{"kind":"native","file":"views/main.json"},
            "web":{"kind":"webview","entry":"web/index.html"}
        }, "contributes":{
            "tools":[{"id":"panel","title":"%panel%","view":"native","scope":"window"}],
            "tabs":[{"id":"browser","title":"Browser","view":"web","state_version":1}],
            "settings":{"schema":"schema.json"}
        }
    });
    fs::write(
        directory.join(MANIFEST_FILE_NAME),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        directory.join("views/main.json"),
        br#"{"schema":1,"root":{"type":"text","text":"%hello%"}}"#,
    )
    .unwrap();
    fs::write(directory.join("schema.json"), br#"{"type":"object"}"#).unwrap();
    fs::write(directory.join("web/index.html"), b"<html>original</html>").unwrap();
    fs::write(directory.join("web/app.css"), b"body{} ").unwrap();
    fs::write(
        directory.join("l10n/en.json"),
        br#"{"hello":"Hello","panel":"Panel"}"#,
    )
    .unwrap();
    fs::write(
        directory.join("l10n/zh-CN.json"),
        "{\"panel\":\"面板\"}".as_bytes(),
    )
    .unwrap();
    install(&store, &id, &version);
    let validator = ProtocolBootValidator {
        host_api: "1.0.0".into(),
        platform: Platform::WindowsX86_64,
    };
    let boot = store.begin_startup(&validator, false).unwrap();
    let prepared = &boot.plugins[0].manifest;
    assert_eq!(prepared.localizations["zh-CN"]["panel"], "面板");
    assert_eq!(prepared.localizations["en"]["hello"], "Hello");
    assert!(
        prepared
            .package
            .configuration_schemas
            .contains_key("schema.json")
    );
    assert!(prepared.native_views.contains_key("native"));
    assert_eq!(
        &*prepared.resources["web/index.html"],
        b"<html>original</html>"
    );
    fs::write(directory.join("web/index.html"), b"changed").unwrap();
    fs::write(directory.join("l10n/en.json"), b"changed").unwrap();
    assert_eq!(
        &*prepared.resources["web/index.html"],
        b"<html>original</html>"
    );
    assert_eq!(prepared.localizations["en"]["hello"], "Hello");
}

#[test]
/// Windows 把仅大小写不同的版本目录视为同一路径，清理时必须保守保留活动版本。
fn cleanup_preserves_case_folded_active_version() {
    use kerminal_domain::plugins::InstalledPlugin;

    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let disk_version = PluginVersion::parse("1.0.0+build").unwrap();
    let active_version = PluginVersion::parse("1.0.0+BUILD").unwrap();
    package(root.path(), &id, &disk_version);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.plugins.insert(
        id.clone(),
        InstalledPlugin {
            active: active_version,
            previous: None,
            enabled: true,
            source: PluginSource::Local,
            installed_at: "2026-11-08T10:00:00Z".into(),
        },
    );
    store.compare_and_swap(&current, next).unwrap();
    let _boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(
        root.path()
            .join("plugins/packages")
            .join(id.as_str())
            .join(disk_version.to_string())
            .exists()
    );
}
