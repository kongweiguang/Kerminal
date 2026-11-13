//! schema 1 → schema 2 插件布局迁移通过公开迁移入口执行，并保持数据 marker 最后写入。
//!
//! @author kongweiguang

use std::path::{Path, PathBuf};

use kerminal_application::data_migration::{
    DataMigrationOutcome, DataMigrationReadiness, DataMigrationRepository, PluginLayoutOutcome,
    PluginLayoutReadiness,
};
use kerminal_domain::plugins::{PluginId, PluginInstallStatus, PluginStoreState};
use kerminal_infrastructure::{
    data_migration::{FileDataMigrationRepository, plugins as plugin_layout},
    plugins::{boot::ProtocolBootValidator, layout, store::PluginStore},
    storage::StorageCoordinator,
};
use kerminal_plugin_protocol::Platform;

/// 夹具复制到临时根，窗口与测试互不影响且不接触默认用户目录。
fn fixture_root(name: &str) -> (tempfile::TempDir, PathBuf) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/plugins-layout-v1")
        .join(name);
    let temp = tempfile::tempdir().expect("隔离数据根");
    let root = temp.path().join("root");
    copy_tree(&source, &root);
    (temp, root)
}

fn copy_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("创建夹具目标");
    for entry in std::fs::read_dir(source).expect("读取夹具目录") {
        let entry = entry.expect("夹具目录项");
        let destination = target.join(entry.file_name());
        let metadata = entry.metadata().expect("夹具元数据");
        if metadata.is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), &destination).expect("复制夹具文件");
        }
    }
}

/// 旧布局必须走同一仓储完成插件转换与数据 marker；再次 apply 只报告已当前。
#[test]
fn legacy_layout_migrates_through_the_public_repository() {
    let (_temp, root) = fixture_root("normal");
    let repository = FileDataMigrationRepository::new(&root);
    let inspection = repository.inspect().expect("只读预检");
    assert_eq!(inspection.plugin_layout, PluginLayoutReadiness::Legacy);
    assert_eq!(inspection.readiness, DataMigrationReadiness::Legacy);

    let report = repository.migrate().expect("迁移成功");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    let plugins = report.plugin_layout.expect("插件布局步骤回执");
    assert_eq!(plugins.outcome, PluginLayoutOutcome::Migrated);
    assert_eq!(plugins.migrated_plugins, 1);
    assert_eq!(plugins.removed_previous_versions, 1);

    let state: PluginStoreState = serde_json::from_slice(
        &std::fs::read(root.join("plugins/state.json")).expect("迁移后状态"),
    )
    .expect("schema 2 状态");
    assert_eq!(state.schema, 2);
    assert_eq!(
        state
            .plugins
            .get(&PluginId::parse("acme.missing").unwrap())
            .expect("缺失记录保留")
            .status,
        PluginInstallStatus::Missing
    );
    assert!(root.join(".kerminal-format.toml").is_file());

    let again = repository.migrate().expect("幂等重跑");
    assert_eq!(again.outcome, DataMigrationOutcome::AlreadyCurrent);
    assert!(again.plugin_layout.is_none());
}

/// 残留与未知材料在公开入口同样阻断，且不能写出 marker 或改写原文。
#[test]
fn residual_material_blocks_without_writing_marker() {
    for fixture in ["residual-staging", "unknown-material"] {
        let (_temp, root) = fixture_root(fixture);
        let repository = FileDataMigrationRepository::new(&root);
        assert_eq!(
            repository.inspect().expect("只读预检").readiness,
            DataMigrationReadiness::Blocked,
            "{fixture}"
        );
        assert!(repository.migrate().is_err(), "{fixture}");
        assert!(!root.join(".kerminal-format.toml").exists(), "{fixture}");
        assert!(!root.join("plugins/.migration").exists(), "{fixture}");
    }
}

/// 安装/更新先写 pending 与 `.staging/<id>/<uuid>`，下次启动必须能重建 journal 并应用；
/// 若分类把该槽当残留，启动快照永远不会执行，安装窗口会卡死普通启动。
#[test]
fn pending_install_with_staging_slot_is_applied_by_startup() {
    let fixture = tempfile::tempdir().expect("隔离数据根");
    let root = fixture.path().join("root");
    let plugins = root.join("plugins");
    let staged = "00000000-0000-4000-8000-000000000021";
    let slot = plugins.join(".staging/acme.notes").join(staged);
    let package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/plugins-layout-v1/normal/plugins/packages/acme.notes/1.0.0");
    copy_tree(&package, &slot);
    let fingerprint = layout::fingerprint_tree(&slot).expect("暂存包摘要");
    std::fs::write(
        plugins.join("state.json"),
        format!(
            r#"{{"schema":2,"revision":3,"plugins":{{}},"pending":[{{"op":"install","id":"acme.notes","to":"1.0.0","staged":"{staged}","fingerprint":"{fingerprint}","source":"official","installed_at":"2026-06-07T08:09:10Z"}}],"failures":{{}}}}"#
        ),
    )
    .expect("写入待安装状态");

    let inspection = plugin_layout::inspect(&root).expect("只读分类");
    assert!(inspection.blockers.is_empty(), "{:?}", inspection.blockers);
    assert_eq!(inspection.readiness(), PluginLayoutReadiness::Current);

    let store = PluginStore::new(StorageCoordinator::new(&root)).expect("插件存储");
    store
        .begin_startup(
            &ProtocolBootValidator {
                host_api: "1.0.0".into(),
                platform: Platform::current().expect("测试平台"),
            },
            false,
        )
        .expect("启动快照应用 pending");
    let state = store.load().expect("读取应用后状态").state;
    assert!(state.pending.is_empty());
    assert_eq!(
        state
            .plugins
            .get(&PluginId::parse("acme.notes").unwrap())
            .expect("安装记录")
            .active,
        "1.0.0"
            .parse::<kerminal_domain::plugins::PluginVersion>()
            .expect("版本")
    );
    assert!(plugins.join("acme.notes/kerminal-plugin.json").is_file());
    assert!(!slot.exists());
}
