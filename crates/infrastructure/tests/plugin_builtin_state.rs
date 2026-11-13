//! 验证内置单元开关、保留身份与外部插件记录的启动边界。
//! @author kongweiguang

use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginId, PluginInstallStatus, PluginSource,
    PluginUnsupportedReason, PluginVersion,
};
use kerminal_infrastructure::{
    plugins::{
        boot::{BootPackageError, BootPackageValidator},
        layout,
        store::{PluginStore, PluginStoreError},
    },
    storage::StorageCoordinator,
};
use uuid::Uuid;

struct FixtureValidator;

impl BootPackageValidator for FixtureValidator {
    type Manifest = String;

    /// 只接受与目录身份一致的最小清单，避免目录存在被误认为可加载包。
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

struct CountingValidator(AtomicUsize);

impl BootPackageValidator for CountingValidator {
    type Manifest = ();

    /// 计数用于证明保留身份在读取包之前就被拒绝，而不是读取失败后归类。
    fn validate(
        &self,
        directory: &Path,
        _id: &PluginId,
        _version: &PluginVersion,
        _source: PluginSource,
    ) -> Result<(), BootPackageError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        fs::read(directory.join("manifest"))
            .map(|_| ())
            .map_err(|_| BootPackageError::Corrupt)
    }
}

/// 每个场景使用独立配置根，不接触用户真实插件状态。
fn store(root: &Path) -> PluginStore {
    PluginStore::new(StorageCoordinator::new(root)).unwrap()
}

fn version(value: &str) -> PluginVersion {
    PluginVersion::parse(value).unwrap()
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

fn installed(root: &Path, id: &PluginId, version: PluginVersion) -> InstalledPlugin {
    package(root, id, &version);
    InstalledPlugin {
        active: version,
        previous: None,
        enabled: true,
        source: PluginSource::Local,
        installed_at: "2026-11-11T00:00:00Z".into(),
        fingerprint: Some(layout::fingerprint_tree(&layout::package_directory(root, id)).unwrap()),
        status: PluginInstallStatus::Ready,
        confirmed: true,
    }
}

fn push_pending(store: &PluginStore, change: PendingPluginChange) {
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.pending.push(change);
    store.compare_and_swap(&current, next).unwrap();
}

/// 合法内置开关与保留身份的外部记录都必须能读回，避免一次启动前的拒绝
/// 把历史记录变成整份状态损坏，导致用户无法卸载。
#[test]
fn legal_builtins_and_reserved_external_records_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let ssh = PluginId::parse("kerminal.ssh").unwrap();
    let logs = PluginId::parse("kerminal.logs").unwrap();
    let core = PluginId::parse("kerminal.core").unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.builtins.insert(ssh.clone(), false);
    next.builtins.insert(logs.clone(), true);
    next.plugins.insert(
        core.clone(),
        installed(temp.path(), &core, version("1.0.0")),
    );
    let committed = store.compare_and_swap(&current, next).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.state, committed.state);
    assert!(!loaded.state.builtins[&ssh]);
    assert!(loaded.state.builtins[&logs]);
    assert!(loaded.state.plugins.contains_key(&core));
}

/// 未知内置键说明状态来自不兼容的写入方；必须保留原文件进入恢复判定，
/// 不能静默丢弃该键后继续启动。
#[test]
fn unknown_builtin_key_is_invalid_and_preserves_file() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let current = store.load().unwrap();
    let mut value = serde_json::to_value(&current.state).unwrap();
    value["builtins"]["acme.unknown"] = serde_json::json!(false);
    std::fs::create_dir_all(temp.path().join("plugins")).unwrap();
    let path = temp.path().join("plugins/state.json");
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(store.load(), Err(PluginStoreError::InvalidState)));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

/// 核心单元停用会使状态无法解释启动根，必须在写入边界直接拒绝。
#[test]
fn disabled_core_builtin_is_invalid_and_preserves_file() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let current = store.load().unwrap();
    let mut value = serde_json::to_value(&current.state).unwrap();
    value["builtins"]["kerminal.core"] = serde_json::json!(false);
    std::fs::create_dir_all(temp.path().join("plugins")).unwrap();
    let path = temp.path().join("plugins/state.json");
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(store.load(), Err(PluginStoreError::InvalidState)));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

/// 内置启停只写开关覆盖项：停用写 false，启用删除覆盖项，重复启动保持幂等。
#[test]
fn builtin_enable_disable_is_idempotent_across_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let validator = FixtureValidator;
    let ssh = PluginId::parse("kerminal.ssh").unwrap();

    let initial = store.begin_startup(&validator, false).unwrap();
    assert!(initial.store.state.builtins.is_empty());

    push_pending(&store, PendingPluginChange::Disable { id: ssh.clone() });
    let disabled = store.begin_startup(&validator, false).unwrap();
    assert!(!disabled.store.state.builtins[&ssh]);
    assert!(disabled.store.state.pending.is_empty());
    assert!(disabled.rejected.is_empty());
    assert!(disabled.plugins.is_empty());

    let again = store.begin_startup(&validator, false).unwrap();
    assert!(!again.store.state.builtins[&ssh]);

    push_pending(&store, PendingPluginChange::Enable { id: ssh.clone() });
    let enabled = store.begin_startup(&validator, false).unwrap();
    assert!(!enabled.store.state.builtins.contains_key(&ssh));
    assert!(enabled.store.state.pending.is_empty());

    push_pending(&store, PendingPluginChange::Enable { id: ssh.clone() });
    let repeated = store.begin_startup(&validator, false).unwrap();
    assert!(!repeated.store.state.builtins.contains_key(&ssh));
    assert!(repeated.rejected.is_empty());
}

/// schema1 迁移可合法产出“缺失目录且无摘要”的保留身份记录；首次启动
/// 必须能写入 Unsupported 状态，而不是让整份 next 状态被校验拒绝。
#[test]
fn missing_reserved_record_without_fingerprint_starts_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let reserved = PluginId::parse("kerminal.core").unwrap();
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.plugins.insert(
        reserved.clone(),
        InstalledPlugin {
            active: version("1.0.0"),
            previous: None,
            enabled: false,
            source: PluginSource::Local,
            installed_at: "2026-11-11T00:00:00Z".into(),
            fingerprint: None,
            status: PluginInstallStatus::Missing,
            confirmed: true,
        },
    );
    store.compare_and_swap(&current, next).unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.rejected[&reserved], BootPackageError::ReservedIdentity);
    let stored = &boot.store.state.plugins[&reserved];
    assert_eq!(
        stored.status,
        PluginInstallStatus::Unsupported {
            reason: PluginUnsupportedReason::ReservedIdentity
        }
    );
    assert!(stored.fingerprint.is_none());
    assert!(boot.store.state.pending.is_empty());
    assert!(!boot.store.state.failures.contains_key(&reserved));
}

/// 核心停用是可记录的插件失败：整个启动继续、pending 保留供撤销，
/// 但不得写入 false 让下一次启动误以为核心真的已停用。
#[test]
fn core_disable_fails_without_mutating_builtins() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let core = PluginId::parse("kerminal.core").unwrap();
    push_pending(&store, PendingPluginChange::Disable { id: core.clone() });

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.rejected[&core], BootPackageError::ImmutableCore);
    assert_eq!(boot.store.state.failures[&core], "核心插件不可停用");
    assert!(boot.store.state.builtins.is_empty());
    assert!(matches!(
        boot.store.state.pending.as_slice(),
        [PendingPluginChange::Disable { id }] if id == &core
    ));
    assert!(boot.plugins.is_empty());
}

/// 占用内置身份的安装、更新、卸载都不会移动目录或创建外部记录，
/// 失败只绑定当前 pending，其他插件仍能正常启动。
#[test]
fn reserved_install_pending_fails_without_touching_directories() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let reserved = PluginId::parse("kerminal.ssh").unwrap();
    let normal = PluginId::parse("acme.notes").unwrap();
    let normal_version = version("1.0.0");
    let normal_record = installed(temp.path(), &normal, normal_version.clone());

    let operation_id = Uuid::new_v4().to_string();
    let staged = layout::staging_directory(temp.path(), &reserved, &operation_id).unwrap();
    fs::create_dir_all(&staged).unwrap();
    fs::write(staged.join("manifest"), format!("{reserved}@2.0.0")).unwrap();
    let fingerprint = layout::fingerprint_tree(&staged).unwrap();

    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.plugins.insert(normal.clone(), normal_record);
    next.pending.push(PendingPluginChange::Install {
        id: reserved.clone(),
        to: version("2.0.0"),
        staged: operation_id.clone(),
        fingerprint: Some(fingerprint),
        source: PluginSource::Local,
        installed_at: "2026-11-11T00:00:00Z".into(),
    });
    store.compare_and_swap(&current, next).unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.rejected[&reserved], BootPackageError::ReservedIdentity);
    assert_eq!(
        boot.store.state.failures[&reserved],
        "插件身份与内置单元冲突"
    );
    assert!(boot.store.state.pending.iter().any(|change| {
        matches!(change, PendingPluginChange::Install { id, .. } if id == &reserved)
    }));
    assert!(!boot.store.state.plugins.contains_key(&reserved));
    assert!(!layout::package_directory(temp.path(), &reserved).exists());
    assert!(staged.exists());
    assert_eq!(boot.plugins.len(), 1);
    assert_eq!(boot.plugins[0].id, normal);
}

/// Update pending 同样不能把保留身份当成已有外部记录来换位；
/// 失败保留 pending 与暂存目录，供用户撤销，不创建新的插件记录。
#[test]
fn reserved_update_pending_fails_without_creating_record() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let reserved = PluginId::parse("kerminal.tmux").unwrap();
    let operation_id = Uuid::new_v4().to_string();
    let staged = layout::staging_directory(temp.path(), &reserved, &operation_id).unwrap();
    fs::create_dir_all(&staged).unwrap();
    fs::write(staged.join("manifest"), format!("{reserved}@2.0.0")).unwrap();
    let fingerprint = layout::fingerprint_tree(&staged).unwrap();
    push_pending(
        &store,
        PendingPluginChange::Update {
            id: reserved.clone(),
            to: version("2.0.0"),
            staged: operation_id,
            fingerprint: Some(fingerprint),
        },
    );

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.rejected[&reserved], BootPackageError::ReservedIdentity);
    assert_eq!(
        boot.store.state.failures[&reserved],
        "插件身份与内置单元冲突"
    );
    assert!(!boot.store.state.plugins.contains_key(&reserved));
    assert!(!layout::package_directory(temp.path(), &reserved).exists());
    assert!(staged.exists());
    assert!(boot.store.state.pending.iter().any(|change| {
        matches!(change, PendingPluginChange::Update { id, .. } if id == &reserved)
    }));
}

/// 旧 pending 中的内置卸载不能进入目录换位，也不能删除外部记录；
/// 记录随后只转为不可支持状态，仍保留卸载/撤销入口。
#[test]
fn reserved_uninstall_pending_preserves_external_record_and_directory() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let reserved = PluginId::parse("kerminal.core").unwrap();
    let record = installed(temp.path(), &reserved, version("1.0.0"));
    let directory = layout::package_directory(temp.path(), &reserved);
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.plugins.insert(reserved.clone(), record);
    let operation_id = Uuid::new_v4().to_string();
    next.pending.push(PendingPluginChange::Uninstall {
        id: reserved.clone(),
        operation_id: operation_id.clone(),
    });
    store.compare_and_swap(&current, next).unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.rejected[&reserved], BootPackageError::ReservedIdentity);
    assert!(boot.store.state.plugins.contains_key(&reserved));
    assert!(directory.exists());
    assert!(
        !layout::backup_directory(temp.path(), &reserved, &operation_id)
            .unwrap()
            .exists()
    );
    assert!(boot.store.state.pending.is_empty());
    assert_eq!(
        boot.store.state.failures[&reserved],
        "插件身份与内置单元冲突"
    );
}

/// 外部记录一旦占用内置身份就不能读取目录，也不能进入启动快照；
/// 记录与目录保留用于卸载，且不能阻止其他插件加载。
#[test]
fn reserved_external_record_is_unsupported_and_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let reserved = PluginId::parse("kerminal.core").unwrap();
    let normal = PluginId::parse("acme.notes").unwrap();
    let normal_record = installed(temp.path(), &normal, version("1.0.0"));
    let reserved_record = installed(temp.path(), &reserved, version("1.0.0"));
    let reserved_directory = layout::package_directory(temp.path(), &reserved);

    let current = store.load().unwrap();
    let mut next = current.state.clone();
    next.plugins.insert(normal.clone(), normal_record);
    next.plugins.insert(reserved.clone(), reserved_record);
    next.failures
        .insert(reserved.clone(), "旧失败记录".to_owned());
    store.compare_and_swap(&current, next).unwrap();

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert_eq!(boot.rejected[&reserved], BootPackageError::ReservedIdentity);
    assert!(boot.plugins.iter().all(|plugin| plugin.id != reserved));
    assert!(boot.plugins.iter().any(|plugin| plugin.id == normal));
    let stored = &boot.store.state.plugins[&reserved];
    assert_eq!(
        stored.status,
        PluginInstallStatus::Unsupported {
            reason: PluginUnsupportedReason::ReservedIdentity
        }
    );
    assert!(!stored.enabled);
    assert!(!boot.store.state.failures.contains_key(&reserved));
    assert!(reserved_directory.exists());
}

/// 离线快照同样在读取包之前拒绝保留身份；计数器为零是“未探测目录”的直接证据。
#[test]
fn static_snapshot_rejects_reserved_identity_without_reading_package() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let reserved = PluginId::parse("kerminal.logs").unwrap();
    let normal = PluginId::parse("acme.notes").unwrap();
    package(temp.path(), &reserved, &version("1.0.0"));
    package(temp.path(), &normal, &version("1.0.0"));
    let current = store.load().unwrap();
    let mut next = current.state.clone();
    for id in [&reserved, &normal] {
        next.plugins
            .insert(id.clone(), installed(temp.path(), id, version("1.0.0")));
    }
    store.compare_and_swap(&current, next).unwrap();

    let validator = CountingValidator(AtomicUsize::new(0));
    let (plugins, rejected) = store.read_static_plugins(&validator).unwrap();
    assert_eq!(rejected[&reserved], BootPackageError::ReservedIdentity);
    assert!(plugins.iter().all(|plugin| plugin.id != reserved));
    assert!(plugins.iter().any(|plugin| plugin.id == normal));
    assert_eq!(validator.0.load(Ordering::SeqCst), 1);
}

/// 保留身份目录名、以及仍引用保留身份的 pending，都不能被认领成 drop-in；
/// 目录保留原样，避免启动扫描把内置能力改写成外部插件。
#[test]
fn discover_skips_reserved_drop_in_directories() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    for name in ["kerminal.core", "kerminal", "kerminal.ssh"] {
        let directory = temp.path().join("plugins").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("manifest"), b"external").unwrap();
        fs::write(
            directory.join("kerminal-plugin.json"),
            serde_json::json!({
                "schema": 1,
                "id": name,
                "version": "1.0.0",
                "runtime": {"kind": "none"}
            })
            .to_string(),
        )
        .unwrap();
    }

    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(boot.store.state.plugins.is_empty());
    assert!(boot.rejected.is_empty());
    assert!(boot.store.state.failures.is_empty());
    assert!(boot.plugins.is_empty());
    for name in ["kerminal.core", "kerminal", "kerminal.ssh"] {
        assert!(temp.path().join("plugins").join(name).exists());
    }
}

/// pending 引用的保留身份目录不能被发现逻辑重复认领成 DropIn 记录。
#[test]
fn pending_reserved_install_does_not_create_record() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let reserved = PluginId::parse("kerminal.logs").unwrap();
    let operation_id = Uuid::new_v4().to_string();
    let staged = layout::staging_directory(temp.path(), &reserved, &operation_id).unwrap();
    fs::create_dir_all(&staged).unwrap();
    fs::write(staged.join("manifest"), b"external").unwrap();
    let fingerprint = layout::fingerprint_tree(&staged).unwrap();

    push_pending(
        &store,
        PendingPluginChange::Install {
            id: reserved.clone(),
            to: version("1.0.0"),
            staged: operation_id,
            fingerprint: Some(fingerprint),
            source: PluginSource::Local,
            installed_at: "2026-11-11T00:00:00Z".into(),
        },
    );
    let boot = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!boot.store.state.plugins.contains_key(&reserved));
    assert_eq!(boot.rejected[&reserved], BootPackageError::ReservedIdentity);
    assert!(boot.store.state.pending.iter().any(|change| {
        matches!(change, PendingPluginChange::Install { id, .. } if id == &reserved)
    }));
}

/// 保留身份原因必须稳定序列化为 snake_case，旧版本读回时才能识别固定类别。
#[test]
fn reserved_identity_reason_uses_snake_case() {
    let reason = PluginInstallStatus::Unsupported {
        reason: PluginUnsupportedReason::ReservedIdentity,
    };
    assert_eq!(
        serde_json::to_value(reason).unwrap(),
        serde_json::json!({"kind": "unsupported", "reason": "reserved_identity"})
    );
}

/// 发现阶段为坏目录写下失败但不产生记录；用户删除目录后，这条没有归属的
/// 文案必须在下一次启动复验时清理，不能永久占着插件 id。
#[test]
fn orphaned_drop_in_failure_is_cleared_after_directory_removal() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(temp.path());
    let broken = PluginId::parse("acme.broken").unwrap();
    let directory = temp.path().join("plugins/acme.broken");
    fs::create_dir_all(&directory).unwrap();

    let first = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!first.store.state.plugins.contains_key(&broken));
    assert_eq!(first.store.state.failures[&broken], "插件清单损坏");
    assert_eq!(first.rejected[&broken], BootPackageError::Corrupt);

    fs::remove_dir_all(&directory).unwrap();
    let second = store.begin_startup(&FixtureValidator, false).unwrap();
    assert!(!second.store.state.failures.contains_key(&broken));
    assert!(!second.store.state.plugins.contains_key(&broken));
    assert!(!second.rejected.contains_key(&broken));
}
