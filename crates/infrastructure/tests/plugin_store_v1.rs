//! 验证单目录布局的 CAS、drop-in 发现、指纹复验与 journal 恢复。
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
};

use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginBootPhase, PluginFingerprint, PluginId,
    PluginInstallStatus, PluginSource, PluginUnsupportedReason, PluginVersion,
};
use kerminal_infrastructure::{
    plugins::{
        boot::{BootPackageError, BootPackageValidator},
        layout::{self, LayoutPhase, PluginLayoutJournal},
        store::{PluginStore, PluginStoreError},
    },
    storage::StorageCoordinator,
};
use uuid::Uuid;

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

/// 单目录夹具：`plugins/<id>/` 直接就是活动版本。
fn package(root: &Path, id: &PluginId, version: &PluginVersion) {
    let directory = layout::package_directory(root, id);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("manifest"), format!("{id}@{version}")).unwrap();
    fs::write(
        directory.join("kerminal-plugin.json"),
        serde_json::json!({
            "schema": 1,
            "id": id.as_str(),
            "version": version.to_string(),
            "runtime": {"kind": "none"}
        })
        .to_string(),
    )
    .unwrap();
}

/// 在按 ID 分组的暂存槽创建待安装内容，返回 UUID 和实际指纹。
fn staged(root: &Path, id: &PluginId, version: &PluginVersion) -> (String, PluginFingerprint) {
    let operation_id = Uuid::new_v4().to_string();
    let directory = layout::staging_directory(root, id, &operation_id).unwrap();
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("manifest"), format!("{id}@{version}")).unwrap();
    fs::write(
        directory.join("kerminal-plugin.json"),
        serde_json::json!({
            "schema": 1,
            "id": id.as_str(),
            "version": version.to_string(),
            "runtime": {"kind": "none"}
        })
        .to_string(),
    )
    .unwrap();
    let fingerprint = layout::fingerprint_tree(&directory).unwrap();
    (operation_id, fingerprint)
}

/// 队列安装意图，让启动实际执行目录换位。
fn queue_install(store: &PluginStore, root: &Path, id: &PluginId, version: &PluginVersion) {
    let (operation_id, fingerprint) = staged(root, id, version);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Install {
        id: id.clone(),
        to: version.clone(),
        staged: operation_id,
        fingerprint: Some(fingerprint),
        source: PluginSource::Local,
        installed_at: "2026-11-08T10:00:00Z".into(),
    });
    store.compare_and_swap(&current, next).unwrap();
}

/// 队列更新意图；活动目录仍在原位，启动时才整目录替换。
fn queue_update(store: &PluginStore, root: &Path, id: &PluginId, version: &PluginVersion) {
    let (operation_id, fingerprint) = staged(root, id, version);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: version.clone(),
        staged: operation_id,
        fingerprint: Some(fingerprint),
    });
    store.compare_and_swap(&current, next).unwrap();
}

fn queue_uninstall(store: &PluginStore, id: &PluginId) {
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Uninstall {
        id: id.clone(),
        operation_id: Uuid::new_v4().to_string(),
    });
    store.compare_and_swap(&current, next).unwrap();
}

fn install_and_boot(store: &PluginStore, root: &Path, id: &PluginId, version: &str) {
    let version = PluginVersion::parse(version).unwrap();
    queue_install(store, root, id, &version);
    store.begin_startup(&FixtureValidator, false).unwrap();
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
    assert_eq!(committed.state.schema, 2);
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
/// schema 1 是迁移前的独立格式；0.4 启动读取不得覆盖或静默解释。
fn schema_one_state_is_rejected_without_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("plugins");
    fs::create_dir(&directory).unwrap();
    let path = directory.join("state.json");
    let bytes = br#"{"schema":1,"revision":0,"plugins":{},"pending":[],"failures":{}}"#;
    fs::write(&path, bytes).unwrap();
    assert!(matches!(
        store(root.path()).load(),
        Err(PluginStoreError::InvalidState)
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
/// 更新失败必须还原旧目录并保留活动版本，同时只隔离该插件的后续操作。
fn failed_update_preserves_active_version_and_other_plugins() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let first = PluginId::parse("acme.notes").unwrap();
    let second = PluginId::parse("acme.tasks").unwrap();
    let old = PluginVersion::parse("1.0.0").unwrap();
    let new = PluginVersion::parse("1.1.0").unwrap();
    install_and_boot(&store, root.path(), &first, "1.0.0");
    install_and_boot(&store, root.path(), &second, "1.0.0");
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.enabled_plugins().count(), 2);

    // 暂存内容校验失败：启动换位后必须回滚到旧目录并保留 pending。
    let (operation_id, _) = staged(root.path(), &first, &new);
    let staging = layout::staging_directory(root.path(), &first, &operation_id).unwrap();
    fs::write(staging.join("manifest"), b"corrupt").unwrap();
    let fingerprint = layout::fingerprint_tree(&staging).unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: first.clone(),
        to: new.clone(),
        staged: operation_id,
        fingerprint: Some(fingerprint),
    });
    store.compare_and_swap(&current, next).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.store.state.plugins[&first].active, old);
    assert_eq!(boot.store.state.pending.len(), 1);
    assert_eq!(boot.rejected[&first], BootPackageError::Corrupt);
    assert_eq!(boot.enabled_plugins().count(), 2);
    // 应用 pending 失败留下失败绑定；另一插件的读取成功不能误删它。
    assert!(boot.store.state.pending_failures.contains(&first));
    assert!(!boot.store.state.pending_failures.contains(&second));
    assert!(layout::package_directory(root.path(), &first).is_dir());

    // 用户取消失败意图后重新暂存；旧目录仍是活动版本，更新可以再执行。
    let mut repaired = boot.store.state.clone();
    repaired.pending.clear();
    repaired.failures.clear();
    repaired.pending_failures.clear();
    store.compare_and_swap(&boot.store, repaired).unwrap();
    fs::remove_dir_all(&staging).unwrap();
    queue_update(&store, root.path(), &first, &new);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.store.state.failures.is_empty());
    assert!(boot.store.state.pending_failures.is_empty());
    assert!(boot.store.state.pending.is_empty());
    assert_eq!(boot.store.state.plugins[&first].active, new);
    assert!(boot.store.state.plugins[&first].previous.is_none());
}

#[test]
/// 已安装包读取失败只记录原因，不与后来排队的操作绑定；修复后读取成功清除失败。
fn package_read_failure_is_not_bound_to_later_pending() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let manifest = layout::package_directory(root.path(), &id).join("manifest");
    fs::write(&manifest, b"corrupt").unwrap();
    let broken = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(broken.rejected[&id], BootPackageError::Corrupt);
    assert!(broken.store.state.failures.contains_key(&id));
    // 包损坏不是可撤销的启动应用失败，不能产生绑定。
    assert!(!broken.store.state.pending_failures.contains(&id));
    fs::write(&manifest, format!("{id}@1.0.0")).unwrap();
    let repaired = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(repaired.store.state.failures.is_empty());
    assert!(repaired.store.state.pending_failures.is_empty());

    fs::write(&manifest, b"corrupt").unwrap();
    store.begin_startup(&FixtureValidator, false).unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Uninstall {
        id: id.clone(),
        operation_id: Uuid::new_v4().to_string(),
    });
    // 旧失败与新排队操作可以共存，但不会被自动绑定，撤销入口据此拒绝。
    let queued = store.compare_and_swap(&current, next).unwrap();
    assert!(queued.state.failures.contains_key(&id));
    assert!(!queued.state.pending_failures.contains(&id));
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!boot.store.state.plugins.contains_key(&id));
    assert!(boot.store.state.pending.is_empty());
    assert!(!layout::package_directory(root.path(), &id).exists());
}

#[test]
/// 同一插件既有失败 pending 又有损坏的已安装包时，读取失败只更新原因并保留应用失败绑定。
fn package_read_failure_preserves_existing_apply_binding() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let active = PluginVersion::parse("0.9.0").unwrap();
    let target = PluginVersion::parse("1.0.0").unwrap();
    install_and_boot(&store, root.path(), &id, "0.9.0");
    let (operation_id, _) = staged(root.path(), &id, &target);
    let staging = layout::staging_directory(root.path(), &id, &operation_id).unwrap();
    fs::write(staging.join("manifest"), b"corrupt").unwrap();
    let fingerprint = layout::fingerprint_tree(&staging).unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: target.clone(),
        staged: operation_id,
        fingerprint: Some(fingerprint),
    });
    store.compare_and_swap(&current, next).unwrap();
    let manifest = layout::package_directory(root.path(), &id).join("manifest");
    fs::write(&manifest, b"corrupt").unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.store.state.pending_failures.contains(&id));
    assert_eq!(
        boot.store.state.failures.get(&id).map(String::as_str),
        Some("插件目录内容已变更")
    );
    assert!(
        boot.store
            .state
            .pending
            .iter()
            .any(|change| change.plugin_id() == &id)
    );
    assert_eq!(boot.store.state.plugins[&id].active, active);
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
/// 过期启动身份的阶段推进必须整笔失败：旧窗口回执既不能推进 revision，
/// 也不能扰动当前启动的冻结阶段或崩溃计数。
fn stale_boot_identity_update_leaves_state_untouched() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let first = store.begin_startup(&FixtureValidator, false).unwrap();
    store
        .mark_boot_phase(&first.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();
    let second = store.begin_startup(&FixtureValidator, false).unwrap();
    let before = store.load().unwrap();
    assert!(matches!(
        store.mark_boot_phase(&first.boot_id, PluginBootPhase::FirstFrame),
        Err(PluginStoreError::Conflict)
    ));
    let after = store.load().unwrap();
    assert_eq!(after.state, before.state);
    let boot = after.state.boot.as_ref().unwrap();
    assert_eq!(boot.id, second.boot_id);
    assert_eq!(boot.phase, PluginBootPhase::Starting);
}

#[test]
/// 阶段推进基于读取到的最新状态做单次 CAS：此前写入的内置开关不能丢失，
/// 一次成功推进只增加一个 revision。
fn phase_update_commits_on_latest_state_with_single_revision() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    let ssh = PluginId::parse("kerminal.ssh").unwrap();
    next.builtins.insert(ssh.clone(), false);
    let updated = store.compare_and_swap(&current, next).unwrap();

    let marked = store
        .mark_boot_phase(&boot.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();
    assert_eq!(marked.state.revision, updated.state.revision + 1);
    assert_eq!(marked.state.builtins.get(&ssh), Some(&false));
    assert_eq!(
        marked.state.boot.as_ref().map(|boot| boot.phase),
        Some(PluginBootPhase::RegistryFrozen)
    );
}

#[test]
/// 正式启动共享同一协调器时，长事务只会让阶段写入排队，不会耗尽跨协调器的
/// 约 1 秒重试上限；这正是插件存储与配置仓储必须使用同一协调器的原因。
fn shared_coordinator_queues_boot_phase_instead_of_lock_timeout() {
    use std::{sync::mpsc, time::Duration};

    let root = tempfile::tempdir().unwrap();
    let storage = StorageCoordinator::new(root.path());
    let store = PluginStore::new(storage.clone()).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let (locked, locked_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let guard = storage.lock().unwrap();
        locked.send(()).unwrap();
        // 超过跨协调器的 100 次 * 10ms 上限，验证共享协调器不会把它报成 Lock。
        std::thread::sleep(Duration::from_millis(1200));
        drop(guard);
    });
    locked_rx.recv().unwrap();
    let marked = store
        .mark_boot_phase(&boot.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();
    holder.join().unwrap();
    assert_eq!(
        marked.state.boot.as_ref().map(|boot| boot.phase),
        Some(PluginBootPhase::RegistryFrozen)
    );
}

#[test]
/// 卸载只移除活动目录，独立数据目录必须保留。
fn uninstall_keeps_plugin_data() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let data = layout::data_directory(root.path(), &id);
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("notes"), b"keep").unwrap();
    queue_uninstall(&store, &id);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!boot.store.state.plugins.contains_key(&id));
    assert!(!layout::package_directory(root.path(), &id).exists());
    assert_eq!(fs::read(data.join("notes")).unwrap(), b"keep");
    assert!(!root.path().join("plugins/.backup").exists());
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// drop-in 目录默认停用且未确认；普通 Enable 必须被拒绝，确认只能走指纹入口。
fn drop_in_is_discovered_disabled_and_plain_enable_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.drop").unwrap();
    package(root.path(), &id, &PluginVersion::parse("1.0.0").unwrap());
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let installed = boot.store.state.plugins.get(&id).unwrap();
    assert!(!installed.enabled);
    assert!(!installed.confirmed);
    assert_eq!(installed.source, PluginSource::DropIn);
    assert_eq!(installed.status, PluginInstallStatus::Ready);
    assert!(installed.fingerprint.is_some());
    assert_eq!(boot.enabled_plugins().count(), 0);

    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending
        .push(PendingPluginChange::Enable { id: id.clone() });
    store.compare_and_swap(&current, next).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let installed = boot.store.state.plugins.get(&id).unwrap();
    assert!(!installed.enabled && !installed.confirmed);
    assert_eq!(installed.status, PluginInstallStatus::Ready);
    assert!(boot.store.state.failures.contains_key(&id));
    assert!(boot.store.state.pending_failures.contains(&id));
    assert_eq!(boot.enabled_plugins().count(), 0);
}

#[test]
/// 目录名与清单 ID 不一致、清单损坏或含链接的目录只写失败记录，不认领也不删除。
fn invalid_drop_in_materials_are_preserved_with_failures() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let mismatch = root.path().join("plugins/acme.one");
    fs::create_dir_all(&mismatch).unwrap();
    fs::write(
        mismatch.join("kerminal-plugin.json"),
        br#"{"id":"acme.two","version":"1.0.0","runtime":{"kind":"none"}}"#,
    )
    .unwrap();
    let broken = root.path().join("plugins/acme.bad");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("kerminal-plugin.json"), b"{broken").unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.plugins.is_empty());
    assert!(boot.store.state.plugins.is_empty());
    assert_eq!(
        boot.store
            .state
            .failures
            .get(&PluginId::parse("acme.one").unwrap())
            .map(String::as_str),
        Some("插件目录名与插件 ID 不一致")
    );
    assert_eq!(
        boot.store
            .state
            .failures
            .get(&PluginId::parse("acme.bad").unwrap())
            .map(String::as_str),
        Some("插件清单损坏")
    );
    assert!(mismatch.exists() && broken.exists());
}

#[test]
/// 记录存在但目录缺失标 Missing 且不加载；记录保留供用户移除。
fn missing_package_directory_is_marked_not_loaded() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.missing").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    package(root.path(), &id, &version);
    let fingerprint =
        layout::fingerprint_tree(&layout::package_directory(root.path(), &id)).unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.plugins.insert(
        id.clone(),
        InstalledPlugin {
            active: version,
            previous: None,
            enabled: true,
            source: PluginSource::Local,
            installed_at: "2026-11-08T10:00:00Z".into(),
            fingerprint: Some(fingerprint),
            status: PluginInstallStatus::Ready,
            confirmed: true,
        },
    );
    store.compare_and_swap(&current, next).unwrap();
    fs::remove_dir_all(layout::package_directory(root.path(), &id)).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.plugins.is_empty());
    assert_eq!(
        boot.store.state.plugins[&id].status,
        PluginInstallStatus::Missing
    );
    assert!(boot.store.state.plugins.contains_key(&id));
}

#[test]
/// 迁移时目录缺失只能写 Missing 且无指纹；该记录可见并能被卸载移除。
fn missing_status_can_omit_fingerprint_and_be_removed() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    let path = root.path().join("plugins/state.json");
    let id = PluginId::parse("acme.missing").unwrap();
    let missing = r#"{"schema":2,"revision":0,"plugins":{"acme.missing":{"active":"1.0.0","enabled":true,"source":"local","installed_at":"2026-11-08T10:00:00Z","status":{"kind":"missing"},"confirmed":true}},"pending":[],"failures":{}}"#;
    fs::write(&path, missing).unwrap();
    let loaded = store.load().unwrap().state;
    assert_eq!(loaded.plugins[&id].status, PluginInstallStatus::Missing);
    assert!(loaded.plugins[&id].fingerprint.is_none());
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.plugins.is_empty());
    assert_eq!(
        boot.store.state.plugins[&id].status,
        PluginInstallStatus::Missing
    );
    queue_uninstall(&store, &id);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!boot.store.state.plugins.contains_key(&id));

    let ready = r#"{"schema":2,"revision":0,"plugins":{"acme.missing":{"active":"1.0.0","enabled":true,"source":"local","installed_at":"2026-11-08T10:00:00Z","status":{"kind":"ready"},"confirmed":true}},"pending":[],"failures":{}}"#;
    fs::write(&path, ready).unwrap();
    assert!(matches!(store.load(), Err(PluginStoreError::InvalidState)));
    assert_eq!(fs::read(&path).unwrap(), ready.as_bytes());
}

#[test]
/// 迁移写出的无指纹 Missing 记录可直接重装同版本或新版本，并保留原安装时间。
fn migrated_missing_record_reinstalls_without_fingerprint() {
    for version in ["1.0.0", "1.1.0"] {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let id = PluginId::parse("acme.notes").unwrap();
        let target = PluginVersion::parse(version).unwrap();
        fs::create_dir_all(root.path().join("plugins")).unwrap();
        fs::write(
            root.path().join("plugins/state.json"),
            br#"{"schema":2,"revision":0,"plugins":{"acme.notes":{"active":"1.0.0","enabled":true,"source":"local","installed_at":"2026-11-08T10:00:00Z","status":{"kind":"missing"},"confirmed":true}},"pending":[],"failures":{}}"#,
        )
        .unwrap();
        queue_update(&store, root.path(), &id, &target);
        let boot = store.begin_startup(&FixtureValidator, false).unwrap();
        let installed = &boot.store.state.plugins[&id];
        assert_eq!(installed.active, target);
        assert_eq!(installed.status, PluginInstallStatus::Ready);
        assert!(installed.fingerprint.is_some());
        assert_eq!(installed.installed_at, "2026-11-08T10:00:00Z");
        assert!(installed.enabled && installed.confirmed);
        assert!(boot.store.state.pending.is_empty());
        assert_eq!(boot.plugins.len(), 1);
        assert!(!root.path().join("plugins/.migration").exists());
    }
}

#[test]
/// 记录指纹尚在但目录被删除时，同版本重装要重建目录而不是被旧指纹挡住。
fn deleted_directory_record_reinstalls_same_version() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let before = store.load().unwrap().state.plugins[&id].clone();
    fs::remove_dir_all(layout::package_directory(root.path(), &id)).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(
        boot.store.state.plugins[&id].status,
        PluginInstallStatus::Missing
    );
    assert_eq!(
        boot.store.state.plugins[&id].fingerprint,
        before.fingerprint
    );

    queue_update(&store, root.path(), &id, &version);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let installed = &boot.store.state.plugins[&id];
    assert_eq!(installed.active, version);
    assert_eq!(installed.status, PluginInstallStatus::Ready);
    assert!(installed.fingerprint.is_some());
    assert_eq!(installed.installed_at, before.installed_at);
    assert!(boot.store.state.pending.is_empty());
    assert_eq!(boot.plugins.len(), 1);
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// Missing 记录重装失败时保持原缺失状态，不伪造指纹，pending 仍可撤销。
fn failed_missing_record_reinstall_keeps_legal_state() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    fs::write(
        root.path().join("plugins/state.json"),
        br#"{"schema":2,"revision":0,"plugins":{"acme.notes":{"active":"1.0.0","enabled":true,"source":"local","installed_at":"2026-11-08T10:00:00Z","status":{"kind":"missing"},"confirmed":true}},"pending":[],"failures":{}}"#,
    )
    .unwrap();
    let target = PluginVersion::parse("1.1.0").unwrap();
    queue_update(&store, root.path(), &id, &target);
    let queued = store.load().unwrap().state;
    let operation_id = match &queued.pending[0] {
        PendingPluginChange::Update { staged, .. } => staged.clone(),
        other => panic!("unexpected pending: {other:?}"),
    };
    fs::write(
        layout::staging_directory(root.path(), &id, &operation_id)
            .unwrap()
            .join("manifest"),
        b"corrupt",
    )
    .unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let failed = store.load().unwrap().state;
    assert_eq!(failed.plugins[&id].status, PluginInstallStatus::Missing);
    assert!(failed.plugins[&id].fingerprint.is_none());
    assert!(failed.pending_failures.contains(&id));
    assert_eq!(failed.pending.len(), 1);
    assert!(failed.failures.contains_key(&id));
    assert!(boot.plugins.is_empty());
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// 指纹不一致时禁用并标 Corrupt，不加载用户已手工替换的目录。
fn fingerprint_change_disables_corrupt_package() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.changed").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    fs::write(
        layout::package_directory(root.path(), &id).join("added.txt"),
        b"unexpected",
    )
    .unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.plugins.is_empty());
    let installed = &boot.store.state.plugins[&id];
    assert_eq!(installed.status, PluginInstallStatus::Corrupt);
    assert!(!installed.enabled);
    assert_eq!(
        boot.store.state.failures.get(&id).map(String::as_str),
        Some("插件目录内容已变更")
    );
}

#[test]
/// 旧 script 清单必须识别为 Unsupport，不当作普通损坏，也不解析视图。
fn script_manifest_is_unsupported_and_can_be_uninstalled() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.legacy").unwrap();
    let directory = layout::package_directory(root.path(), &id);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("kerminal-plugin.json"),
        br#"{"id":"acme.legacy","version":"1.0.0","runtime":{"kind":"script"}}"#,
    )
    .unwrap();
    let fingerprint = layout::fingerprint_tree(&directory).unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.plugins.insert(
        id.clone(),
        InstalledPlugin {
            active: PluginVersion::parse("1.0.0").unwrap(),
            previous: None,
            enabled: true,
            source: PluginSource::Local,
            installed_at: "2026-11-08T10:00:00Z".into(),
            fingerprint: Some(fingerprint),
            status: PluginInstallStatus::Ready,
            confirmed: true,
        },
    );
    store.compare_and_swap(&current, next).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(
        boot.store.state.plugins[&id].status,
        PluginInstallStatus::Unsupported {
            reason: PluginUnsupportedReason::ScriptRemoved,
        }
    );
    assert_eq!(boot.rejected[&id], BootPackageError::Unsupported);
    queue_uninstall(&store, &id);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!boot.store.state.plugins.contains_key(&id));
    assert!(!layout::package_directory(root.path(), &id).exists());
}

#[test]
/// 同 ID 的不同安装 UUID 各自保留证据；启动只消费 pending 指向的那个槽。
fn same_id_staging_slots_do_not_overwrite_each_other() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    let (first, _) = staged(root.path(), &id, &version);
    let (second, _) = staged(root.path(), &id, &version);
    let first = layout::staging_directory(root.path(), &id, &first).unwrap();
    let second = layout::staging_directory(root.path(), &id, &second).unwrap();
    assert!(first.is_dir() && second.is_dir());
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Install {
        id: id.clone(),
        to: version.clone(),
        staged: first.file_name().unwrap().to_string_lossy().into_owned(),
        fingerprint: Some(layout::fingerprint_tree(&first).unwrap()),
        source: PluginSource::Local,
        installed_at: "2026-11-08T10:00:00Z".into(),
    });
    store.compare_and_swap(&current, next).unwrap();
    store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(layout::package_directory(root.path(), &id).is_dir());
    // 未被引用的另一槽没有 journal 证明，不能盲删。
    assert!(second.is_dir());
}

#[test]
/// 崩溃在旧目录已移走之后重启，journal 证据应前滚完成更新并清理备份。
fn old_moved_journal_rolls_forward_update() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let new = PluginVersion::parse("1.1.0").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let before = store.load().unwrap().state.plugins[&id]
        .fingerprint
        .clone()
        .unwrap();
    let (operation_id, after) = staged(root.path(), &id, &new);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: new.clone(),
        staged: operation_id.clone(),
        fingerprint: Some(after.clone()),
    });
    store.compare_and_swap(&current, next).unwrap();
    let mut journal = PluginLayoutJournal::update(
        root.path(),
        &id,
        &operation_id,
        before,
        after.clone(),
        after,
    )
    .unwrap();
    layout::write_journal(root.path(), &journal).unwrap();
    let backup = layout::backup_directory(root.path(), &id, &operation_id).unwrap();
    layout::move_directory(
        &layout::package_directory(root.path(), &id),
        &backup,
        &operation_id,
    )
    .unwrap();
    journal.phase = LayoutPhase::OldMoved;
    layout::write_journal(root.path(), &journal).unwrap();
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.store.state.plugins[&id].active, new);
    assert!(boot.store.state.pending.is_empty());
    assert!(!backup.exists());
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// Update 在 staging 已移到 final、阶段仍写 OldMoved 时重启，恢复应幂等前滚。
fn old_moved_journal_after_staging_move_rolls_forward() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let new = PluginVersion::parse("1.1.0").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let before = store.load().unwrap().state.plugins[&id]
        .fingerprint
        .clone()
        .unwrap();
    let (operation_id, after) = staged(root.path(), &id, &new);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: new.clone(),
        staged: operation_id.clone(),
        fingerprint: Some(after.clone()),
    });
    store.compare_and_swap(&current, next).unwrap();
    let mut journal = PluginLayoutJournal::update(
        root.path(),
        &id,
        &operation_id,
        before,
        after.clone(),
        after,
    )
    .unwrap();
    layout::write_journal(root.path(), &journal).unwrap();
    let backup = layout::backup_directory(root.path(), &id, &operation_id).unwrap();
    layout::move_directory(
        &layout::package_directory(root.path(), &id),
        &backup,
        &operation_id,
    )
    .unwrap();
    layout::move_directory(
        &layout::staging_directory(root.path(), &id, &operation_id).unwrap(),
        &layout::package_directory(root.path(), &id),
        &operation_id,
    )
    .unwrap();
    journal.phase = LayoutPhase::OldMoved;
    layout::write_journal(root.path(), &journal).unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.store.state.plugins[&id].active, new);
    assert!(boot.store.state.pending.is_empty());
    assert!(!backup.exists());
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// Install 在改名后、推进 journal 前崩溃：一次启动前滚完成，不把活动目录误认成 drop-in。
fn prepared_install_journal_with_moved_staging_rolls_forward() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    queue_install(&store, root.path(), &id, &version);
    let queued = store.load().unwrap().state;
    let (operation_id, after) = match &queued.pending[0] {
        PendingPluginChange::Install {
            staged,
            fingerprint: Some(fingerprint),
            ..
        } => (staged.clone(), fingerprint.clone()),
        other => panic!("unexpected pending: {other:?}"),
    };
    let journal = PluginLayoutJournal::install(
        root.path(),
        &id,
        &operation_id,
        after.clone(),
        after.clone(),
    )
    .unwrap();
    layout::write_journal(root.path(), &journal).unwrap();
    layout::move_directory(
        &layout::staging_directory(root.path(), &id, &operation_id).unwrap(),
        &layout::package_directory(root.path(), &id),
        &operation_id,
    )
    .unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.store.state.plugins[&id].active, version);
    assert_eq!(boot.store.state.plugins[&id].source, PluginSource::Local);
    assert!(boot.store.state.pending.is_empty());
    assert_eq!(boot.plugins.len(), 1);
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// journal 已到 NewInstalled、staging 已被移走且状态未提交时，重试必须幂等完成安装。
fn new_installed_install_journal_replays_without_staging() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    queue_install(&store, root.path(), &id, &version);
    let queued = store.load().unwrap().state;
    let (operation_id, after) = match &queued.pending[0] {
        PendingPluginChange::Install {
            staged,
            fingerprint: Some(fingerprint),
            ..
        } => (staged.clone(), fingerprint.clone()),
        other => panic!("unexpected pending: {other:?}"),
    };
    let mut journal = PluginLayoutJournal::install(
        root.path(),
        &id,
        &operation_id,
        after.clone(),
        after.clone(),
    )
    .unwrap();
    layout::write_journal(root.path(), &journal).unwrap();
    layout::move_directory(
        &layout::staging_directory(root.path(), &id, &operation_id).unwrap(),
        &layout::package_directory(root.path(), &id),
        &operation_id,
    )
    .unwrap();
    journal.phase = LayoutPhase::NewInstalled;
    layout::write_journal(root.path(), &journal).unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.store.state.plugins[&id].active, version);
    assert!(boot.store.state.pending.is_empty());
    assert!(boot.store.state.failures.is_empty());
    assert_eq!(boot.plugins.len(), 1);
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// 活动目录与备份都不存在时按可记录失败处理，不留下挡住撤销的空 journal。
fn update_without_active_or_backup_fails_without_journal() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let old = PluginVersion::parse("1.0.0").unwrap();
    let new = PluginVersion::parse("1.1.0").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    queue_update(&store, root.path(), &id, &new);
    let queued = store.load().unwrap().state;
    let operation_id = match &queued.pending[0] {
        PendingPluginChange::Update { staged, .. } => staged.clone(),
        other => panic!("unexpected pending: {other:?}"),
    };
    let slot = layout::staging_directory(root.path(), &id, &operation_id).unwrap();
    fs::remove_dir_all(layout::package_directory(root.path(), &id)).unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let failed = store.load().unwrap().state;
    assert_eq!(failed.plugins[&id].active, old);
    assert!(failed.pending_failures.contains(&id));
    assert_eq!(
        failed.failures.get(&id).map(String::as_str),
        Some("插件目录不存在")
    );
    assert_eq!(failed.pending.len(), 1);
    assert!(slot.is_dir());
    assert!(!root.path().join("plugins/.migration").exists());
    assert!(boot.plugins.is_empty());
}

#[test]
/// 活动路径被替换成普通文件时卸载只记插件失败，不能留下 fatal journal。
fn uninstall_preflight_rejects_unsafe_active_path() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let active = layout::package_directory(root.path(), &id);
    fs::remove_dir_all(&active).unwrap();
    fs::write(&active, b"foreign").unwrap();
    queue_uninstall(&store, &id);

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    let failed = store.load().unwrap().state;
    assert!(failed.plugins.contains_key(&id));
    assert!(failed.pending_failures.contains(&id));
    assert_eq!(failed.pending.len(), 1);
    assert_eq!(fs::read(&active).unwrap(), b"foreign");
    assert!(!root.path().join("plugins/.migration").exists());
    assert!(boot.plugins.is_empty());
}

#[test]
/// 启动清理无引用的 `_incoming` 解包残留，但不能触碰 pending 引用的暂存槽。
fn startup_cleans_incoming_residue_only() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let version = PluginVersion::parse("1.0.0").unwrap();
    queue_install(&store, root.path(), &id, &version);
    let queued = store.load().unwrap().state;
    let operation_id = match &queued.pending[0] {
        PendingPluginChange::Install { staged, .. } => staged.clone(),
        other => panic!("unexpected pending: {other:?}"),
    };
    let slot = layout::staging_directory(root.path(), &id, &operation_id).unwrap();
    fs::write(slot.join("manifest"), b"corrupt").unwrap();
    let incoming = root.path().join("plugins/.staging/_incoming");
    let residue = incoming.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&residue).unwrap();
    fs::write(residue.join("archive"), b"leftover").unwrap();
    fs::write(incoming.join("stray-file"), b"leftover").unwrap();

    store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(fs::read_dir(&incoming).unwrap().count(), 0);
    assert!(slot.is_dir());
    assert!(store.load().unwrap().state.pending_failures.contains(&id));
}

#[test]
/// 非规范 UUID、drop-in 安装意图和同 ID 重复 pending 都必须整份拒绝。
fn invalid_pending_shape_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    let path = root.path().join("plugins/state.json");
    let fingerprint = format!("v1:{}", "0".repeat(64));
    for state in [
        format!(
            r#"{{"schema":2,"revision":0,"plugins":{{}},"pending":[{{"op":"install","id":"acme.notes","to":"1.0.0","staged":"urn:uuid:11111111-1111-1111-1111-111111111111","fingerprint":"{fingerprint}","source":"local","installed_at":"2026-11-08T10:00:00Z"}}],"failures":{{}}}}"#
        ),
        format!(
            r#"{{"schema":2,"revision":0,"plugins":{{}},"pending":[{{"op":"update","id":"acme.notes","to":"1.1.0","staged":"11111111111111111111111111111111","fingerprint":"{fingerprint}"}}],"failures":{{}}}}"#
        ),
        r#"{"schema":2,"revision":0,"plugins":{},"pending":[{"op":"uninstall","id":"acme.notes","operation_id":"{11111111-1111-1111-1111-111111111111}"}],"failures":{}}"#.to_owned(),
        format!(
            r#"{{"schema":2,"revision":0,"plugins":{{}},"pending":[{{"op":"install","id":"acme.notes","to":"1.0.0","staged":"11111111-1111-1111-1111-111111111111","fingerprint":"{fingerprint}","source":"dropin","installed_at":"2026-11-08T10:00:00Z"}}],"failures":{{}}}}"#
        ),
        r#"{"schema":2,"revision":0,"plugins":{},"pending":[{"op":"uninstall","id":"acme.notes","operation_id":"11111111-1111-1111-1111-111111111111"},{"op":"uninstall","id":"acme.notes","operation_id":"22222222-2222-2222-2222-222222222222"}],"failures":{}}"#.to_owned(),
    ] {
        fs::write(&path, state.as_bytes()).unwrap();
        assert!(matches!(store.load(), Err(PluginStoreError::InvalidState)));
        assert_eq!(fs::read(&path).unwrap(), state.as_bytes());
    }
}

#[cfg(windows)]
#[test]
/// Committed journal 只剩备份清理时不能阻止 mark_boot_phase；下次启动继续清理。
fn committed_cleanup_failure_does_not_block_boot_phase() {
    use std::os::windows::fs::OpenOptionsExt;

    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let new = PluginVersion::parse("1.1.0").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let before = store.load().unwrap().state.plugins[&id]
        .fingerprint
        .clone()
        .unwrap();
    let (operation_id, after) = staged(root.path(), &id, &new);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: new.clone(),
        staged: operation_id.clone(),
        fingerprint: Some(after.clone()),
    });
    store.compare_and_swap(&current, next).unwrap();
    let mut journal = PluginLayoutJournal::update(
        root.path(),
        &id,
        &operation_id,
        before,
        after.clone(),
        after,
    )
    .unwrap();
    layout::write_journal(root.path(), &journal).unwrap();
    let backup = layout::backup_directory(root.path(), &id, &operation_id).unwrap();
    layout::move_directory(
        &layout::package_directory(root.path(), &id),
        &backup,
        &operation_id,
    )
    .unwrap();
    layout::move_directory(
        &layout::staging_directory(root.path(), &id, &operation_id).unwrap(),
        &layout::package_directory(root.path(), &id),
        &operation_id,
    )
    .unwrap();
    journal.phase = LayoutPhase::OldMoved;
    layout::write_journal(root.path(), &journal).unwrap();
    // 允许读写但不共享删除，稳定复现提交后的备份清理失败。
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(backup.join("kerminal-plugin.json"))
        .unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.cleanup_failed);
    assert!(backup.is_dir());
    assert!(root.path().join("plugins/.migration").is_dir());
    store
        .mark_boot_phase(&boot.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();

    drop(held);
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!boot.cleanup_failed);
    assert!(!backup.exists());
    assert!(!root.path().join("plugins/.migration").exists());
}

#[test]
/// staging 丢失时证据不足，必须返回恢复错误并保留备份、journal 与旧状态。
fn missing_staging_evidence_requires_recovery() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let id = PluginId::parse("acme.notes").unwrap();
    let old = PluginVersion::parse("1.0.0").unwrap();
    let new = PluginVersion::parse("1.1.0").unwrap();
    install_and_boot(&store, root.path(), &id, "1.0.0");
    let before = store.load().unwrap().state.plugins[&id]
        .fingerprint
        .clone()
        .unwrap();
    let (operation_id, after) = staged(root.path(), &id, &new);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: new,
        staged: operation_id.clone(),
        fingerprint: Some(after.clone()),
    });
    store.compare_and_swap(&current, next).unwrap();
    let mut journal = PluginLayoutJournal::update(
        root.path(),
        &id,
        &operation_id,
        before,
        after.clone(),
        after,
    )
    .unwrap();
    layout::write_journal(root.path(), &journal).unwrap();
    let backup = layout::backup_directory(root.path(), &id, &operation_id).unwrap();
    layout::move_directory(
        &layout::package_directory(root.path(), &id),
        &backup,
        &operation_id,
    )
    .unwrap();
    journal.phase = LayoutPhase::OldMoved;
    layout::write_journal(root.path(), &journal).unwrap();
    fs::remove_dir_all(layout::staging_directory(root.path(), &id, &operation_id).unwrap())
        .unwrap();
    assert!(matches!(
        store.begin_startup(&FixtureValidator, false),
        Err(PluginStoreError::LayoutRecoveryRequired { .. })
    ));
    assert!(backup.is_dir());
    assert!(root.path().join("plugins/.migration").is_dir());
    assert_eq!(store.load().unwrap().state.plugins[&id].active, old);
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
    let (operation_id, _) = staged(root.path(), &id, &version);
    let staging = layout::staging_directory(root.path(), &id, &operation_id).unwrap();
    fs::remove_file(staging.join("kerminal-plugin.json")).unwrap();
    fs::create_dir_all(staging.join("views")).unwrap();
    fs::create_dir_all(staging.join("web")).unwrap();
    fs::create_dir_all(staging.join("l10n")).unwrap();
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
        staging.join(MANIFEST_FILE_NAME),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        staging.join("views/main.json"),
        br#"{"schema":1,"root":{"type":"text","text":"%hello%"}}"#,
    )
    .unwrap();
    fs::write(staging.join("schema.json"), br#"{"type":"object"}"#).unwrap();
    fs::write(staging.join("web/index.html"), b"<html>original</html>").unwrap();
    fs::write(staging.join("web/app.css"), b"body{} ").unwrap();
    fs::write(
        staging.join("l10n/en.json"),
        br#"{"hello":"Hello","panel":"Panel"}"#,
    )
    .unwrap();
    fs::write(
        staging.join("l10n/zh-CN.json"),
        "{\"panel\":\"面板\"}".as_bytes(),
    )
    .unwrap();
    let fingerprint = layout::fingerprint_tree(&staging).unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(PendingPluginChange::Install {
        id: id.clone(),
        to: version.clone(),
        staged: operation_id,
        fingerprint: Some(fingerprint),
        source: PluginSource::Local,
        installed_at: "2026-11-08T10:00:00Z".into(),
    });
    store.compare_and_swap(&current, next).unwrap();
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
    let directory = layout::package_directory(root.path(), &id);
    fs::write(directory.join("web/index.html"), b"changed").unwrap();
    fs::write(directory.join("l10n/en.json"), b"changed").unwrap();
    assert_eq!(
        &*prepared.resources["web/index.html"],
        b"<html>original</html>"
    );
    assert_eq!(prepared.localizations["en"]["hello"], "Hello");
}

#[test]
/// 伪造绑定缺少 failure 或 pending 时整份状态必须拒绝，避免撤销凭据漂移。
fn forged_failure_binding_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    let path = root.path().join("plugins/state.json");
    for state in [
        // 绑定 id 既没有 failure 也没有 pending。
        r#"{"schema":2,"revision":0,"plugins":{},"pending":[],"failures":{},"pending_failures":["acme.notes"]}"#.to_owned(),
        // 有失败记录但已无 pending，绑定不能再声称可撤销。
        r#"{"schema":2,"revision":0,"plugins":{},"pending":[],"failures":{"acme.notes":"安装失败"},"pending_failures":["acme.notes"]}"#.to_owned(),
        // 有 pending 但缺少失败记录，绑定不能凭空生成撤销入口。
        r#"{"schema":2,"revision":0,"plugins":{},"pending":[{"op":"uninstall","id":"acme.notes","operation_id":"11111111-1111-1111-1111-111111111111"}],"failures":{},"pending_failures":["acme.notes"]}"#.to_owned(),
        // 回退已移除；schema 2 中出现 rollback 必须整份拒绝。
        r#"{"schema":2,"revision":0,"plugins":{"acme.notes":{"active":"1.0.0","enabled":true,"source":"local","installed_at":"2026-11-08T10:00:00Z","fingerprint":"v1:0000000000000000000000000000000000000000000000000000000000000000","status":{"kind":"ready"},"confirmed":true}},"pending":[{"op":"rollback","id":"acme.notes","to":"0.9.0"}],"failures":{}}"#.to_owned(),
    ] {
        fs::write(&path, state).unwrap();
        assert!(matches!(
            store.load(),
            Err(PluginStoreError::InvalidState)
        ));
    }
}

#[test]
/// 插件身份直接用作目录名，保留名与 Windows 设备名必须在领域入口拒绝。
fn plugin_identity_rejects_reserved_storage_names() {
    for value in [
        "state.json",
        "acme.state.json",
        "state-recovery-1.json",
        "con.plugin",
        "aux.tool",
        "lpt9.plugin",
        "com1.plugin",
        "prn.plugin",
        "nul.plugin",
    ] {
        assert!(
            PluginId::parse(value).is_err(),
            "{value} must not become a storage segment"
        );
    }
    for value in ["acme.notes", "acme.state", "console.plugin", "com0.plugin"] {
        let id = PluginId::parse(value).unwrap();
        assert_eq!(id.storage_segment(), value);
    }
}

#[test]
/// 状态缺失时不创建配置根，离线快照保持只读默认 schema。
fn missing_root_is_not_created_by_load() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("missing");
    let store = PluginStore::new(StorageCoordinator::new(&path)).unwrap();
    let (plugins, rejected) = store.read_static_plugins(&FixtureValidator).unwrap();
    assert!(plugins.is_empty() && rejected.is_empty());
    assert!(!path.exists());
}

/// 供未使用警告抑制：测试夹具保留目录构造器引用。
#[allow(dead_code)]
fn package_path(root: &Path, id: &PluginId) -> PathBuf {
    layout::package_directory(root, id)
}
