//! 失败 pending 的显式撤销必须可重装，且不能覆盖并发状态或丢弃未知提交材料。
//! @author kongweiguang

use super::*;
use crate::plugins::layout;
use kerminal_application::PublicErrorCode;

/// 使用真实本地归档和独立配置根，避免恢复测试依赖网络或用户现有插件。
fn fixture() -> (
    tempfile::TempDir,
    PluginStore,
    PackageInstaller,
    ProtocolBootValidator,
) {
    let root = tempfile::tempdir().unwrap();
    let config = crate::configuration::ConfigurationStore::initialize(root.path()).unwrap();
    let store = PluginStore::new(config.storage_coordinator()).unwrap();
    let settings = Arc::new(PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(crate::vault::VaultStore::open(config.storage_coordinator())),
    ));
    let catalog = Arc::new(CatalogClient::testing(
        config.storage_coordinator(),
        "http://127.0.0.1/catalog.json".into(),
        String::new(),
    ));
    let platform = Platform::current().unwrap();
    let installer =
        PackageInstaller::new(store.clone(), catalog, settings, "1.0.0".into(), platform);
    let validator = ProtocolBootValidator {
        host_api: "1.0.0".into(),
        platform,
    };
    (root, store, installer, validator)
}

/// 单版本布局的活动目录固定为 `plugins/<id>`；测试只按契约路径取现场。
fn active_directory(root: &Path) -> PathBuf {
    layout::package_directory(root, &PluginId::parse("acme.notes").unwrap())
}

/// 失败 pending 绑定的暂存槽是安装当时的 UUID 目录，撤销只能清理它。
fn staging_slot(root: &Path, state: &PluginStoreState) -> PathBuf {
    let change = state.pending.first().expect("failed pending exists");
    let (id, operation_id) = match change {
        PendingPluginChange::Install { id, staged, .. }
        | PendingPluginChange::Update { id, staged, .. } => (id, staged),
        other => panic!("unexpected pending: {other:?}"),
    };
    layout::staging_directory(root, id, operation_id).unwrap()
}

/// 篡改已排队的暂存清单，让下次启动在应用 pending 时按指纹失败。
fn corrupt_staged_manifest(root: &Path, state: &PluginStoreState) {
    fs::write(
        staging_slot(root, state).join("kerminal-plugin.json"),
        b"corrupt",
    )
    .unwrap();
}

/// 临时根必须命中真实磁盘空间，溢出与真实可用量不足都不能绕过安装空间检查。
#[test]
fn temporary_root_staging_checks_actual_disk_capacity() {
    let (root, _, installer, _) = fixture();
    let stage = installer
        .stage(1)
        .expect("resolve the temporary directory disk");
    assert!(stage.directory.is_dir());
    let canonical = fs::canonicalize(root.path().join("plugins/.staging/_incoming")).unwrap();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let disk = disks
        .list()
        .iter()
        .filter(|disk| path_on_mount(&canonical, disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .expect("temporary root has an identified disk");
    let exceeds_free_space = disk.available_space().checked_add(1).unwrap();
    let error = installer.stage(exceeds_free_space).err().unwrap();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert!(error.public_message().contains("空间不足"));
    let error = installer.stage(u64::MAX).err().unwrap();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert!(error.public_message().contains("大小无效"));
}

/// Windows 的普通挂载点与扩展规范路径应等价，且相似目录名不能误判为同一挂载点。
#[cfg(windows)]
#[test]
fn windows_mount_matching_accepts_verbatim_paths_and_checks_component_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let mount = root.path().join("mount");
    let adjacent = root.path().join("mount-other");
    fs::create_dir(&mount).unwrap();
    fs::create_dir(&adjacent).unwrap();
    let canonical = fs::canonicalize(&mount).unwrap();
    assert!(canonical.to_string_lossy().starts_with(r"\\?\"));
    assert!(path_on_mount(&canonical, &mount));
    assert!(!path_on_mount(
        &fs::canonicalize(&adjacent).unwrap(),
        &mount
    ));
    assert!(!path_on_mount(&canonical, &root.path().join("missing")));
}

/// 复用生产预检与确认入口，保证重装不是直接构造 pending 的测试假象。
async fn install_local(installer: &PackageInstaller, root: &Path, version: &str) {
    let path = root.join(format!("local-{version}.kplugin"));
    fs::write(&path, package(version)).unwrap();
    let inspected = installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    installer
        .install_file(path, inspected, progress, CancellationToken::new())
        .await
        .unwrap();
}

/// 以启动实际产生的失败记录为基线，校验取消只作用于页面看到的完整操作。
fn cancellation(state: &PluginStoreState) -> ExtensionChange {
    ExtensionChange::CancelFailedChange {
        expected_revision: state.revision,
        pending: state.pending[0].clone(),
    }
}

/// 新装失败不能永远卡住 pending；取消后同一版本能重新走完整校验并启动。
#[tokio::test]
async fn failed_install_can_cancel_and_reinstall_same_version() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    corrupt_staged_manifest(root.path(), &queued);
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    assert_eq!(failed.failures.len(), 1);
    let id = PluginId::parse("acme.notes").unwrap();
    assert!(failed.pending_failures.contains(&id));
    let receipt = installer.change(cancellation(&failed)).await.unwrap();
    assert!(!receipt.restart_required);
    assert!(receipt.cleanup_warning.is_none());
    let cancelled = store.load().unwrap().state;
    assert_eq!(cancelled.revision, receipt.revision);
    assert!(cancelled.pending.is_empty() && cancelled.failures.is_empty());
    assert!(cancelled.pending_failures.is_empty());
    assert!(cancelled.plugins.is_empty());
    assert!(!slot.exists());
    let replay = installer.change(cancellation(&failed)).await.unwrap_err();
    assert_eq!(replay.public_code(), PublicErrorCode::Conflict);
    assert_eq!(store.load().unwrap().state, cancelled);
    install_local(&installer, root.path(), "1.0.0").await;
    let boot = store.begin_startup(&validator, false).unwrap();
    assert_eq!(boot.plugins.len(), 1);
    assert!(boot.store.state.pending.is_empty() && boot.store.state.failures.is_empty());
}

/// 更新失败保留活动版本与用户数据，清理本次暂存后允许立即重新安装。
#[tokio::test]
async fn failed_update_preserves_active_and_data_before_reinstall() {
    let (root, store, installer, validator) = fixture();
    for version in ["0.9.0", "1.0.0"] {
        install_local(&installer, root.path(), version).await;
        store.begin_startup(&validator, false).unwrap();
    }
    let data = root.path().join("plugins/data/acme.notes/keep");
    fs::create_dir_all(data.parent().unwrap()).unwrap();
    fs::write(&data, b"keep").unwrap();
    install_local(&installer, root.path(), "1.1.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    corrupt_staged_manifest(root.path(), &queued);
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    assert_eq!(
        failed.plugins[&PluginId::parse("acme.notes").unwrap()].active,
        PluginVersion::parse("1.0.0").unwrap()
    );
    installer.change(cancellation(&failed)).await.unwrap();
    let cancelled = store.load().unwrap().state;
    assert_eq!(cancelled.plugins, failed.plugins);
    assert!(cancelled.pending.is_empty() && cancelled.failures.is_empty());
    assert_eq!(fs::read(data).unwrap(), b"keep");
    assert!(!slot.exists());
    install_local(&installer, root.path(), "1.1.0").await;
    let boot = store.begin_startup(&validator, false).unwrap();
    assert_eq!(
        boot.plugins[0].version,
        PluginVersion::parse("1.1.0").unwrap()
    );
    assert!(boot.store.state.pending.is_empty() && boot.store.state.failures.is_empty());
}

/// 回装旧版本失败时也必须保留当前活动目录，不能把旧 UUID 当作可回退版本。
#[tokio::test]
async fn cancel_failed_downgrade_preserves_active_package() {
    let (root, store, installer, validator) = fixture();
    for version in ["0.9.0", "1.0.0"] {
        install_local(&installer, root.path(), version).await;
        store.begin_startup(&validator, false).unwrap();
    }
    let active = active_directory(root.path());
    let manifest = active.join("kerminal-plugin.json");
    let original = fs::read(&manifest).unwrap();
    install_local(&installer, root.path(), "0.9.0").await;
    let queued = store.load().unwrap().state;
    corrupt_staged_manifest(root.path(), &queued);
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    installer.change(cancellation(&failed)).await.unwrap();
    let cancelled = store.load().unwrap().state;
    assert_eq!(cancelled.plugins, failed.plugins);
    assert!(cancelled.pending.is_empty() && cancelled.failures.is_empty());
    assert_eq!(
        cancelled.plugins[&PluginId::parse("acme.notes").unwrap()].active,
        PluginVersion::parse("1.0.0").unwrap()
    );
    assert_eq!(fs::read(&manifest).unwrap(), original);
}

/// 没有启动失败证据或身份不匹配时不能取消，避免旧页面撤销另一笔有效操作。
#[tokio::test]
async fn cancel_rejects_unfailed_or_mismatched_pending() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    let queued = store.load().unwrap().state;
    let error = installer.change(cancellation(&queued)).await.unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(store.load().unwrap().state, queued);
    corrupt_staged_manifest(root.path(), &queued);
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    let mut mismatch = failed.pending[0].clone();
    if let PendingPluginChange::Install { to, .. } = &mut mismatch {
        *to = PluginVersion::parse("1.1.0").unwrap();
    }
    let error = installer
        .change(ExtensionChange::CancelFailedChange {
            expected_revision: failed.revision,
            pending: mismatch,
        })
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(store.load().unwrap().state, failed);
}

/// 包读取失败没有启动应用绑定，后来排队的卸载不能被旧失败撤销，状态必须原样保留。
#[tokio::test]
async fn unbound_package_failure_cannot_cancel_later_pending() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    store.begin_startup(&validator, false).unwrap();
    let id = PluginId::parse("acme.notes").unwrap();
    fs::write(
        active_directory(root.path()).join("kerminal-plugin.json"),
        b"corrupt",
    )
    .unwrap();
    store.begin_startup(&validator, false).unwrap();
    let broken = store.load().unwrap().state;
    assert!(broken.failures.contains_key(&id));
    assert!(broken.pending.is_empty() && broken.pending_failures.is_empty());
    installer
        .change(ExtensionChange::Uninstall(id.clone()))
        .await
        .unwrap();
    let queued = store.load().unwrap().state;
    assert!(queued.failures.contains_key(&id));
    assert!(!queued.pending_failures.contains(&id));
    let error = installer.change(cancellation(&queued)).await.unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(store.load().unwrap().state, queued);
}

/// 用户快照过期与读后并发 CAS 两种冲突都必须先失败，不能先清理暂存再发现冲突。
#[tokio::test]
async fn cancel_rejects_revision_and_cas_conflicts_before_cleanup() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    corrupt_staged_manifest(root.path(), &queued);
    store.begin_startup(&validator, false).unwrap();
    let stale = store.load().unwrap();
    let latest = store.compare_and_swap(&stale, stale.state.clone()).unwrap();
    let error = installer
        .change(cancellation(&stale.state))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    let error = super::super::recovery::cancel_failed_change(
        &store,
        &stale,
        stale.state.revision,
        &stale.state.pending[0],
    )
    .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(store.load().unwrap().state, latest.state);
    assert!(slot.exists());
}

/// 未知提交材料即使不影响 load 也必须阻止取消与目录清理，不能自动丢弃或重放。
#[tokio::test]
async fn unknown_recovery_fences_failed_cancellation_and_cleanup() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    corrupt_staged_manifest(root.path(), &queued);
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    let recovery = root.path().join("plugins/state-recovery-test.json");
    fs::write(&recovery, b"unknown commit material").unwrap();
    let error = installer.change(cancellation(&failed)).await.unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(store.load().unwrap().state, failed);
    assert_eq!(fs::read(recovery).unwrap(), b"unknown commit material");
    assert!(slot.exists());
}

/// 活动目录被手工占用且内容不同时只记插件失败，撤销能清掉暂存而不碰用户目录。
#[tokio::test]
async fn occupied_active_directory_fails_pending_without_journal() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    let active = active_directory(root.path());
    fs::create_dir_all(&active).unwrap();
    fs::write(active.join("manifest"), b"acme.notes@0.9.0").unwrap();
    fs::write(active.join("kerminal-plugin.json"), b"user-owned").unwrap();
    let original: Vec<_> = ["manifest", "kerminal-plugin.json"]
        .into_iter()
        .map(|name| fs::read(active.join(name)).unwrap())
        .collect();

    let boot = store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    let id = PluginId::parse("acme.notes").unwrap();
    assert!(boot.plugins.is_empty());
    assert!(failed.pending_failures.contains(&id));
    assert!(failed.plugins.is_empty());
    assert!(!root.path().join("plugins/.migration").exists());
    assert_eq!(fs::read(active.join("manifest")).unwrap(), original[0]);
    assert_eq!(
        fs::read(active.join("kerminal-plugin.json")).unwrap(),
        original[1]
    );

    let receipt = installer.change(cancellation(&failed)).await.unwrap();
    assert!(receipt.cleanup_warning.is_none());
    assert!(store.load().unwrap().state.pending.is_empty());
    assert!(!slot.exists());
    assert!(active.is_dir());
    assert_eq!(fs::read(active.join("manifest")).unwrap(), original[0]);
    assert!(!root.path().join("plugins/.migration").exists());
}

/// 危险残留应阻止本次撤销清理，但同 ID 的新操作可用独立 UUID 槽继续重装。
#[tokio::test]
async fn file_residue_allows_cancel_then_reinstall_or_uninstall() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    store.begin_startup(&validator, false).unwrap();
    install_local(&installer, root.path(), "1.1.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    fs::remove_dir_all(&slot).unwrap();
    fs::write(&slot, b"retain unsafe residue").unwrap();
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    let receipt = installer.change(cancellation(&failed)).await.unwrap();
    assert!(receipt.cleanup_warning.is_some());
    let cancelled = store.load().unwrap().state;
    assert_eq!(cancelled.plugins, failed.plugins);
    assert!(cancelled.pending.is_empty() && cancelled.failures.is_empty());
    assert_eq!(fs::read(&slot).unwrap(), b"retain unsafe residue");

    // 同 ID 的新操作使用独立 UUID 槽，旧残留不能再阻塞重装。
    install_local(&installer, root.path(), "1.1.0").await;
    let boot = store.begin_startup(&validator, false).unwrap();
    assert_eq!(
        boot.plugins[0].version,
        PluginVersion::parse("1.1.0").unwrap()
    );
    assert_eq!(fs::read(&slot).unwrap(), b"retain unsafe residue");

    installer
        .change(ExtensionChange::Uninstall(
            PluginId::parse("acme.notes").unwrap(),
        ))
        .await
        .unwrap();
    let boot = store.begin_startup(&validator, false).unwrap();
    assert!(boot.plugins.is_empty());
    assert_eq!(fs::read(&slot).unwrap(), b"retain unsafe residue");
}

/// Windows 文件被其他进程占用时清理可能失败，但状态撤销仍必须提交并报告残留。
#[cfg(windows)]
#[tokio::test]
async fn cleanup_io_failure_reports_warning_without_locking_failed_change() {
    use std::os::windows::fs::OpenOptionsExt;

    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    let manifest = slot.join("kerminal-plugin.json");
    corrupt_staged_manifest(root.path(), &queued);
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    // 允许读取和写入但不共享删除，稳定复现 Windows 清理的权限错误。
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(&manifest)
        .unwrap();
    let receipt = installer.change(cancellation(&failed)).await.unwrap();
    assert!(receipt.cleanup_warning.is_some());
    let cancelled = store.load().unwrap().state;
    assert_eq!(cancelled.plugins, failed.plugins);
    assert!(cancelled.pending.is_empty() && cancelled.failures.is_empty());
    assert_eq!(fs::read(&manifest).unwrap(), b"corrupt");
    drop(held);
}

/// Unix 链接残留不能被递归清理，撤销成功后外部目标仍保持原始内容。
#[cfg(unix)]
#[tokio::test]
async fn symlink_residue_is_retained_and_external_target_is_untouched() {
    let (root, store, installer, validator) = fixture();
    install_local(&installer, root.path(), "1.0.0").await;
    let queued = store.load().unwrap().state;
    let slot = staging_slot(root.path(), &queued);
    corrupt_staged_manifest(root.path(), &queued);
    let external = tempfile::tempdir().unwrap();
    let target = external.path().join("keep");
    fs::write(&target, b"outside").unwrap();
    std::os::unix::fs::symlink(&target, slot.join("linked")).unwrap();
    store.begin_startup(&validator, false).unwrap();
    let failed = store.load().unwrap().state;
    let receipt = installer.change(cancellation(&failed)).await.unwrap();
    assert!(receipt.cleanup_warning.is_some());
    assert!(store.load().unwrap().state.pending.is_empty());
    assert!(
        slot.join("linked")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(target).unwrap(), b"outside");
}
