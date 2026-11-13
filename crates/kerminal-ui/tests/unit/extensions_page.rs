//! 扩展页选择只使用当前平台的受信资产和语义版本。
//! @author kongweiguang

use super::{
    BuiltinRow, CatalogPlugin, DropInState, ExtensionsPage, Filter, Operation, SelectedEntry,
    StoredActionsPolicy, builtin_display_name, builtin_name_pair, builtin_rows,
    cancelable_failed_change, drop_in_confirmable, drop_in_confirmation_operation, drop_in_state,
    extension_total, installed_total, is_reserved_identity, pending_only_failure, sample,
    selection, stored_actions_policy, unconfirmed_drop_in,
};
use async_trait::async_trait;
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    extensions::{
        BuiltinUnit, CatalogSelection, CatalogSnapshot, CatalogSource, DropInInspection,
        ExtensionChange, ExtensionReceipt, ExtensionsService, InstallProgress,
        LocalPackageInspection, PluginCatalog, PluginPackageStore,
    },
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginFingerprint, PluginId, PluginInstallStatus,
    PluginSource, PluginStoreState, PluginUnsupportedReason, PluginVersion,
};
use kerminal_plugin_protocol::Platform;
use serde_json::json;
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::watch,
};
use tokio_util::sync::CancellationToken;

/// 较新的不兼容版本不能覆盖当前平台可安装的旧版本。
#[test]
fn selects_latest_compatible_asset_by_semver() {
    let entry: CatalogPlugin = serde_json::from_value(json!({
        "id":"acme.notes", "name":{"en":"Notes"}, "description":{"en":"Notes"},
        "publisher":"acme", "categories":[], "icon_url":null, "readme_url":null,
        "repository":"https://example.com", "permissions":[], "versions":[
            {"version":"1.9.0", "api":"^1.0", "published_at":"2026-01-01", "changelog_url":null,
             "assets":[{"platform":"any", "url":"https://example.com/a", "sha256":"a", "size":1}]},
            {"version":"1.10.0", "api":"^1.0", "published_at":"2026-01-02", "changelog_url":null,
             "assets":[{"platform":"any", "url":"https://example.com/b", "sha256":"b", "size":1}]},
            {"version":"2.0.0", "api":"^2.0", "published_at":"2026-01-03", "changelog_url":null,
             "assets":[{"platform":"any", "url":"https://example.com/c", "sha256":"c", "size":1}]}
        ]
    }))
    .unwrap();
    let selected = selection(&entry, "1.0.0").unwrap();
    assert_eq!(selected.version.to_string(), "1.10.0");
    assert_eq!(selected.platform, Platform::Any);
}

/// 没有匹配资产时不能让用户启动安装。
#[test]
fn missing_compatible_asset_disables_install() {
    let entry: CatalogPlugin = serde_json::from_value(json!({
        "id":"acme.notes", "name":{"en":"Notes"}, "description":{"en":"Notes"},
        "publisher":"acme", "categories":[], "icon_url":null, "readme_url":null,
        "repository":"https://example.com", "permissions":[], "versions":[]
    }))
    .unwrap();
    assert!(selection(&entry, "1.0.0").is_none());
}

/// 内嵌示例必须持续符合 v1 目录契约；字段漂移会在测试阶段暴露而不是渲染成空列表。
#[test]
fn sample_catalog_matches_preview_contract() {
    let catalog = sample::catalog();
    assert_eq!(catalog.schema, 1);
    assert_eq!(catalog.source_id, "example-preview");
    assert_eq!(catalog.sequence, 0);

    let ids: Vec<&str> = catalog
        .plugins
        .iter()
        .map(|plugin| plugin.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["example.notes", "example.http-check", "example.file-search"]
    );

    for plugin in &catalog.plugins {
        assert!(sample::contains(&plugin.id));
        assert_eq!(plugin.publisher, "example");
        assert_eq!(plugin.categories, ["示例"]);
        assert!(plugin.name.contains_key("zh-CN") && plugin.name.contains_key("en"));
        assert!(plugin.description.contains_key("zh-CN") && plugin.description.contains_key("en"));
        assert!(!plugin.permissions.is_empty());
        assert_eq!(
            plugin.repository,
            "https://github.com/kongweiguang/Kerminal-Plugins"
        );

        let version = plugin.versions.first().expect("示例必须声明版本");
        assert_eq!(version.version.to_string(), "1.0.0");
        assert_eq!(version.api, "^1.0");
        assert_eq!(version.published_at, "2026-11-08");
        // 示例没有可下载资产：URL 指向仓库、摘要全零、大小 1，安装路径不可达。
        for asset in &version.assets {
            assert_eq!(asset.platform, Platform::Any);
            assert_eq!(asset.url, plugin.repository);
            assert_eq!(asset.sha256, "0".repeat(64));
            assert_eq!(asset.size, 1);
        }
        let selected = selection(plugin, "1.0.0").expect("示例应能识别 any 资产");
        assert_eq!(selected.version.to_string(), "1.0.0");
        // API 范围必须真实生效，示例不能绕过宿主版本约束。
        assert!(selection(plugin, "2.0.0").is_none());
    }
}

/// 示例身份只在真实目录缺席时成立；真实目录即使包含同 id 也必须走普通目录路径。
#[test]
fn preview_guard_requires_missing_catalog_and_sample_id() {
    let sample_id = PluginId::parse("example.notes").unwrap();
    assert!(sample::is_preview_entry(false, &sample_id));
    assert!(!sample::is_preview_entry(true, &sample_id));

    let other = PluginId::parse("acme.notes").unwrap();
    assert!(!sample::is_preview_entry(false, &other));
}

/// pending-only 失败仍须保留失败撤销分支；一旦已有 installed 条目则交给 installed 详情。
#[test]
fn pending_only_failure_keeps_cancel_path() {
    let state: PluginStoreState = serde_json::from_value(json!({
        "schema": 1,
        "revision": 3,
        "plugins": {},
        "pending": [{
            "op": "install",
            "id": "acme.file-search",
            "to": "1.0.0",
            "staged": "packages/acme.file-search/1.0.0",
            "source": "local",
            "installed_at": "2026-11-10"
        }],
        "failures": {"acme.file-search": "安装失败"}
    }))
    .unwrap();
    let id = PluginId::parse("acme.file-search").unwrap();
    assert_eq!(pending_only_failure(&state, &id), Some("安装失败"));

    let installed: PluginStoreState = serde_json::from_value(json!({
        "schema": 1,
        "revision": 4,
        "plugins": {
            "acme.file-search": {
                "active": "1.0.0",
                "previous": null,
                "enabled": true,
                "source": "local",
                "installed_at": "2026-11-10"
            }
        },
        "pending": [],
        "failures": {"acme.file-search": "安装失败"}
    }))
    .unwrap();
    assert_eq!(pending_only_failure(&installed, &id), None);
}

/// 撤销入口取决于失败是否绑定当前 pending：包读取失败的旧记录仍显示原因，但不能撤销。
#[test]
fn cancel_requires_failure_bound_to_pending() {
    let pending = json!({
        "op": "install",
        "id": "acme.file-search",
        "to": "1.0.0",
        "staged": "packages/acme.file-search/1.0.0",
        "source": "local",
        "installed_at": "2026-11-10"
    });
    let id = PluginId::parse("acme.file-search").unwrap();
    let bound: PluginStoreState = serde_json::from_value(json!({
        "schema": 1,
        "revision": 5,
        "plugins": {},
        "pending": [pending.clone()],
        "failures": {"acme.file-search": "安装失败"},
        "pending_failures": ["acme.file-search"]
    }))
    .unwrap();
    assert!(cancelable_failed_change(&bound, &id).is_some());
    assert_eq!(pending_only_failure(&bound, &id), Some("安装失败"));

    let unbound: PluginStoreState = serde_json::from_value(json!({
        "schema": 1,
        "revision": 6,
        "plugins": {},
        "pending": [pending],
        "failures": {"acme.file-search": "安装失败"}
    }))
    .unwrap();
    assert!(cancelable_failed_change(&unbound, &id).is_none());
    // 失败原因仍可展示，只是不提供撤销，避免把旧失败错绑到新排队的操作。
    assert_eq!(pending_only_failure(&unbound, &id), Some("安装失败"));
}

/// 固定指纹只用于观察与确认的绑定，不代表真实目录内容。
fn fingerprint() -> String {
    format!("v1:{}", "a".repeat(64))
}

/// 构造一次只读观察，覆盖确认门禁依赖的状态、指纹与修订。
fn drop_in_inspection(
    id: &str,
    status: PluginInstallStatus,
    fingerprint: Option<String>,
    revision: u64,
    confirmed: bool,
) -> DropInInspection {
    DropInInspection {
        id: PluginId::parse(id).unwrap(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        name: Default::default(),
        permissions: vec!["ui.notify".to_owned()],
        fingerprint,
        enabled: confirmed,
        confirmed,
        status,
        revision,
    }
}

/// 已安装记录直接构造领域结构；Ready 记录必须带指纹，其他状态按磁盘证据缺省。
fn installed_record(source: PluginSource, status: PluginInstallStatus) -> InstalledPlugin {
    InstalledPlugin {
        active: PluginVersion::parse("1.0.0").unwrap(),
        previous: None,
        enabled: false,
        source,
        installed_at: "2026-11-11".to_owned(),
        fingerprint: (status == PluginInstallStatus::Ready)
            .then(|| PluginFingerprint::parse(fingerprint()).unwrap()),
        status,
        confirmed: source != PluginSource::DropIn,
    }
}

/// 通过真实内置注册表拿到 13 个单元，避免测试复制一份会漂移的名单。
fn builtin_units_for_test() -> Vec<BuiltinUnit> {
    let service = ExtensionsService::new(
        Arc::new(FakeCatalog::default()),
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
    );
    service.builtin_units().expect("内置注册表应可枚举")
}

/// 内置行同时受“全部/已安装”、搜索与权威启停表控制；核心不提供停用路径。
#[test]
fn builtin_rows_follow_filters_search_and_authoritative_state() {
    let units = builtin_units_for_test();
    assert_eq!(units.len(), 13);
    let core = PluginId::parse("kerminal.core").unwrap();
    let sftp = PluginId::parse("kerminal.sftp").unwrap();
    assert!(
        !units
            .iter()
            .find(|unit| unit.id == core)
            .expect("核心单元存在")
            .can_disable
    );

    let enabled = PluginStoreState::default();
    let rows: Vec<BuiltinRow> = builtin_rows(&units, Some(&enabled), "", &Filter::All);
    assert_eq!(rows.len(), 13);
    assert_eq!(
        builtin_rows(&units, Some(&enabled), "", &Filter::Installed).len(),
        13
    );
    assert!(builtin_rows(&units, Some(&enabled), "", &Filter::Updates).is_empty());
    assert!(
        builtin_rows(
            &units,
            Some(&enabled),
            "",
            &Filter::Category("示例".to_owned())
        )
        .is_empty()
    );

    // 搜索同时覆盖稳定身份与当前语言展示名，界面看到什么就能搜到什么。
    let by_id = builtin_rows(&units, Some(&enabled), "sftp", &Filter::All);
    assert_eq!(by_id.len(), 1);
    assert_eq!(by_id[0].id, sftp);
    let ports = units
        .iter()
        .find(|unit| unit.id.as_str() == "kerminal.ports")
        .expect("端口转发单元存在");
    let ports_name = builtin_display_name(ports);
    let by_name = builtin_rows(
        &units,
        Some(&enabled),
        &ports_name.to_lowercase(),
        &Filter::All,
    );
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].id.as_str(), "kerminal.ports");
    assert_eq!(by_name[0].name, ports_name);
    // 中文界面按 id 本地化，不能直接显示应用层的英文注册表名。
    let serial = PluginId::parse("kerminal.serial").unwrap();
    assert_eq!(builtin_name_pair(&serial, "Serial").0, "串口");
    assert_eq!(
        builtin_name_pair(
            &PluginId::parse("kerminal.ports").unwrap(),
            "Port forwarding"
        )
        .0,
        "端口转发"
    );
    assert_eq!(
        builtin_display_name(ports),
        crate::locale::text("端口转发", "Port forwarding").to_string()
    );
    // 未知 id 保留注册表名，新增单元不会因缺少映射变成空白行。
    let unknown = BuiltinUnit {
        id: PluginId::parse("kerminal.future").unwrap(),
        name: "Future".to_owned(),
        can_disable: true,
    };
    assert_eq!(builtin_display_name(&unknown), "Future");

    let mut disabled = enabled.clone();
    disabled.builtins.insert(sftp.clone(), false);
    let row = builtin_rows(&units, Some(&disabled), "kerminal.sftp", &Filter::All);
    assert!(!row[0].enabled);
    assert!(!row[0].pending);

    // 排队启停后必须显示重启生效，而不是把旧启停状态当成最终结果。
    disabled
        .pending
        .push(PendingPluginChange::Enable { id: sftp.clone() });
    let row = builtin_rows(&units, Some(&disabled), "kerminal.sftp", &Filter::All);
    assert!(row[0].pending);
    assert!(!row[0].enabled);
}

/// 已安装计数与页头总数都按身份去重，冲突记录不能把同名内置身份算两次。
#[test]
fn installed_counts_union_builtin_and_stored_identities() {
    let units = builtin_units_for_test();
    assert_eq!(installed_total(None, &units), 13);
    assert_eq!(extension_total(&[], &units), 13);

    let mut state = PluginStoreState::default();
    state.plugins.insert(
        PluginId::parse("kerminal.sftp").unwrap(),
        installed_record(
            PluginSource::Local,
            PluginInstallStatus::Unsupported {
                reason: PluginUnsupportedReason::ReservedIdentity,
            },
        ),
    );
    assert_eq!(installed_total(Some(&state), &units), 13);

    state.plugins.insert(
        PluginId::parse("acme.notes").unwrap(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    assert_eq!(installed_total(Some(&state), &units), 14);
}

/// 未确认 drop-in 只有就绪且带指纹的观察才能确认；缺失、损坏与身份冲突一律无动作。
#[test]
fn drop_in_actions_require_ready_observation_and_reserve_conflicts() {
    let id = "acme.notes";
    let ready = drop_in_inspection(
        id,
        PluginInstallStatus::Ready,
        Some(fingerprint()),
        7,
        false,
    );
    assert!(drop_in_confirmable(&ready));
    let expected_fingerprint = fingerprint();
    match drop_in_confirmation_operation(&ready).expect("就绪观察应能构造确认") {
        Operation::ConfirmDropIn {
            id: target,
            fingerprint: frozen,
            revision,
        } => {
            assert_eq!(target.as_str(), id);
            assert_eq!(frozen, expected_fingerprint);
            assert_eq!(revision, 7);
        }
        _ => panic!("确认动作必须是确认 drop-in"),
    }

    let missing = drop_in_inspection(id, PluginInstallStatus::Missing, None, 7, false);
    let corrupt = drop_in_inspection(
        id,
        PluginInstallStatus::Corrupt,
        Some(fingerprint()),
        7,
        false,
    );
    assert!(!drop_in_confirmable(&missing));
    assert!(!drop_in_confirmable(&corrupt));
    assert!(drop_in_confirmation_operation(&missing).is_none());
    assert!(drop_in_confirmation_operation(&corrupt).is_none());

    let unconfirmed = installed_record(PluginSource::DropIn, PluginInstallStatus::Ready);
    assert_eq!(
        drop_in_state(&unconfirmed, Some(&ready)),
        DropInState::PendingConfirmation
    );
    assert_eq!(
        stored_actions_policy(&unconfirmed, Some(&ready)),
        StoredActionsPolicy::ConfirmDropIn
    );
    assert_eq!(
        stored_actions_policy(&unconfirmed, Some(&missing)),
        StoredActionsPolicy::Unavailable
    );
    assert_eq!(
        stored_actions_policy(&unconfirmed, Some(&corrupt)),
        StoredActionsPolicy::Unavailable
    );

    let reserved = installed_record(
        PluginSource::Local,
        PluginInstallStatus::Unsupported {
            reason: PluginUnsupportedReason::ReservedIdentity,
        },
    );
    assert!(is_reserved_identity(&reserved));
    assert_eq!(
        drop_in_state(&reserved, None),
        DropInState::ReservedIdentity
    );
    assert_eq!(
        stored_actions_policy(&reserved, None),
        StoredActionsPolicy::None
    );
}

/// 手动投放插件只读观察数据；测试用内存记录确认动作实际携带的参数。
#[derive(Default)]
struct FakeCatalog {
    readme_calls: Arc<AtomicUsize>,
    /// 显式刷新需要成功回执时注入快照；默认失败让只读激活路径保持可观察的离线状态。
    refresh_snapshot: Arc<Mutex<Option<CatalogSnapshot>>>,
}

#[async_trait]
impl CatalogSource for FakeCatalog {
    /// 测试页面直接注入目录快照；缓存读取保持为空，避免触发真实 I/O。
    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>> {
        Ok(None)
    }

    /// 计数 README 请求是断言“内置与 drop-in 不发网络”的唯一证据。
    async fn readme(
        &self,
        _id: &PluginId,
        _cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        self.readme_calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    /// 只有注入快照的用例走成功刷新；默认失败好过伪造一份可安装目录。
    async fn refresh(&self, _cancellation: CancellationToken) -> AppResult<CatalogSnapshot> {
        self.refresh_snapshot
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::invalid("测试不请求市场目录"))
    }
}

/// 手动投放的只读状态与确认回执都保存在内存，测试可观察每次调用参数与读回次数。
struct FakeStore {
    state: Arc<Mutex<PluginStoreState>>,
    inspections: Arc<Mutex<Vec<DropInInspection>>>,
    state_reads: Arc<AtomicUsize>,
    inspect_reads: Arc<AtomicUsize>,
    confirm_calls: Arc<Mutex<Vec<(PluginId, String, u64)>>>,
    conflict_confirmation: Arc<AtomicBool>,
}

impl FakeStore {
    /// 每份夹具独立持有状态与计数器，避免并行测试共享可变证据。
    fn new(state: PluginStoreState, inspections: Vec<DropInInspection>) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            inspections: Arc::new(Mutex::new(inspections)),
            state_reads: Arc::new(AtomicUsize::new(0)),
            inspect_reads: Arc::new(AtomicUsize::new(0)),
            confirm_calls: Arc::new(Mutex::new(Vec::new())),
            conflict_confirmation: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl PluginPackageStore for FakeStore {
    /// 每次读取都计数，用于证明冲突后确实重新回读了权威状态。
    async fn state(&self) -> AppResult<PluginStoreState> {
        self.state_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.state.lock().unwrap().clone())
    }

    async fn install_catalog(
        &self,
        _selection: CatalogSelection,
        _progress: watch::Sender<InstallProgress>,
        _cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        Err(AppError::invalid("测试不安装目录包"))
    }

    async fn inspect_file(
        &self,
        _path: PathBuf,
        _cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection> {
        Err(AppError::invalid("测试不预检本地包"))
    }

    async fn install_file(
        &self,
        _path: PathBuf,
        _inspected: LocalPackageInspection,
        _progress: watch::Sender<InstallProgress>,
        _cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        Err(AppError::invalid("测试不安装本地包"))
    }

    /// 内置启停写入同一权威状态，供可见行断言重启生效。
    async fn change(&self, change: ExtensionChange) -> AppResult<ExtensionReceipt> {
        let (id, enabled) = match change {
            ExtensionChange::Enable(id) => (id, true),
            ExtensionChange::Disable(id) => (id, false),
            _ => return Err(AppError::invalid("测试不支持该变更")),
        };
        let mut state = self.state.lock().unwrap();
        if let Some(record) = state.plugins.get_mut(&id) {
            record.enabled = enabled;
        }
        state.builtins.insert(id.clone(), enabled);
        state.pending.push(if enabled {
            PendingPluginChange::Enable { id }
        } else {
            PendingPluginChange::Disable { id }
        });
        state.revision += 1;
        Ok(ExtensionReceipt {
            revision: state.revision,
            restart_required: true,
            cleanup_warning: None,
        })
    }

    async fn inspect_drop_ins(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<DropInInspection>> {
        self.inspect_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.inspections.lock().unwrap().clone())
    }

    /// 记录调用参数后按测试开关返回成功或冲突；成功时同步推进记录与观察。
    async fn confirm_drop_in(
        &self,
        id: PluginId,
        expected_fingerprint: String,
        expected_revision: u64,
    ) -> AppResult<ExtensionReceipt> {
        self.confirm_calls.lock().unwrap().push((
            id.clone(),
            expected_fingerprint,
            expected_revision,
        ));
        if self.conflict_confirmation.load(Ordering::SeqCst) {
            return Err(AppError::conflict("插件状态已更改"));
        }
        let mut state = self.state.lock().unwrap();
        if state.revision != expected_revision {
            return Err(AppError::conflict("插件状态已更改"));
        }
        let record = state
            .plugins
            .get_mut(&id)
            .ok_or_else(|| AppError::not_found("测试插件不存在"))?;
        record.confirmed = true;
        record.enabled = true;
        state
            .pending
            .push(PendingPluginChange::Enable { id: id.clone() });
        state.revision += 1;
        let revision = state.revision;
        drop(state);
        if let Some(inspection) = self
            .inspections
            .lock()
            .unwrap()
            .iter_mut()
            .find(|inspection| inspection.id == id)
        {
            inspection.confirmed = true;
            inspection.enabled = true;
            inspection.revision = revision;
        }
        Ok(ExtensionReceipt {
            revision,
            restart_required: true,
            cleanup_warning: None,
        })
    }

    async fn delete_data(&self, _id: PluginId) -> AppResult<()> {
        Err(AppError::invalid("测试不删除数据"))
    }
}

/// 空目录快照用于证明 README 请求来自选择动作，而不是页面构造或目录读取。
fn empty_catalog_snapshot() -> CatalogSnapshot {
    CatalogSnapshot {
        catalog: PluginCatalog {
            schema: 1,
            source_id: "test".to_owned(),
            sequence: 0,
            plugins: Vec::new(),
        },
        stale: false,
        fetched_at: 0,
    }
}

/// 目录条目只保留稳定身份与一个可安装版本，测试聚焦详情优先级而非市场契约。
fn catalog_plugin(id: &str) -> CatalogPlugin {
    serde_json::from_value(json!({
        "id": id,
        "name": {"en": id},
        "description": {"en": id},
        "publisher": "acme",
        "categories": [],
        "icon_url": null,
        "readme_url": null,
        "repository": "https://example.com",
        "permissions": [],
        "versions": [{
            "version": "1.0.0",
            "api": "^1.0",
            "published_at": "2026-01-01",
            "changelog_url": null,
            "assets": [{
                "platform": "any",
                "url": "https://example.com/package",
                "sha256": "a",
                "size": 1
            }]
        }]
    }))
    .expect("测试目录条目应可解析")
}

/// 带目录条目的快照用于证明同 id 时记录详情与目录行不会同时出现。
fn catalog_snapshot(plugins: Vec<CatalogPlugin>) -> CatalogSnapshot {
    CatalogSnapshot {
        catalog: PluginCatalog {
            schema: 1,
            source_id: "test".to_owned(),
            sequence: 0,
            plugins,
        },
        stale: false,
        fetched_at: 0,
    }
}

/// 在真实窗口挂载生产扩展页；服务与运行时只存在于当前 headless 测试。
fn page_fixture(
    app: &mut TestAppContext,
    store: Arc<FakeStore>,
    catalog: Arc<FakeCatalog>,
) -> (Runtime, Entity<ExtensionsPage>, &mut VisualTestContext) {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试运行时");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let service = Arc::new(ExtensionsService::new(catalog, store));
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let page =
            cx.new(|cx| ExtensionsPage::new(service, executor, "1.0.0".to_owned(), window, cx));
        *mounted.borrow_mut() = Some(page.clone());
        gpui_kit::base::Root::new(page, window, cx)
    });
    visual.simulate_resize(size(px(900.), px(600.)));
    let page = slot.borrow_mut().take().expect("扩展页已挂载");
    (runtime, page, visual)
}

/// 交替推进 GPUI 与 Tokio，直到页面回执与观察重读都落地。
fn pump(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..6 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
}

/// 选中内置或 drop-in 只切换详情；只有普通目录条目才会向 README 服务发起读取。
#[gpui_kit::test]
fn selecting_builtin_or_drop_in_skips_readme(app: &mut TestAppContext) {
    let drop_in = drop_in_inspection(
        "acme.notes",
        PluginInstallStatus::Ready,
        Some(fingerprint()),
        3,
        false,
    );
    let mut state = PluginStoreState {
        revision: 3,
        ..Default::default()
    };
    state.plugins.insert(
        drop_in.id.clone(),
        installed_record(PluginSource::DropIn, PluginInstallStatus::Ready),
    );
    let store = Arc::new(FakeStore::new(state.clone(), vec![drop_in.clone()]));
    let catalog = Arc::new(FakeCatalog::default());
    let calls = catalog.readme_calls.clone();
    let (runtime, page, visual) = page_fixture(app, store, catalog);

    let builtin = BuiltinUnit {
        id: PluginId::parse("kerminal.ssh").unwrap(),
        name: "SSH".to_owned(),
        can_disable: true,
    };
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.builtin_units = vec![builtin.clone()];
            page.state = Some(state.clone());
            page.catalog = Some(empty_catalog_snapshot());
            page.select(SelectedEntry::Builtin(builtin.id.clone()), cx);
        });
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(drop_in.id.clone()), cx)
        });
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // 对照：已解析的普通目录条目仍会进入 README 路径，证明上面的零计数不是假阴性。
    let other = PluginId::parse("acme.other").unwrap();
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            // README 只服务于 selected_plugin 已解析的目录条目；无条目的身份按详情优先级跳过。
            page.catalog = Some(catalog_snapshot(vec![catalog_plugin(other.as_str())]));
            page.select(SelectedEntry::Plugin(other), cx)
        });
    });
    pump(&runtime, visual);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// 全启用状态下 13 个内置单元都进入“全部/已安装”列表；搜索与 pending 不产生重复行。
#[gpui_kit::test]
fn visible_rows_append_builtin_units_without_duplicate_pending_rows(app: &mut TestAppContext) {
    let units = builtin_units_for_test();
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.builtin_units = units.clone();
            page.state = Some(PluginStoreState::default());
        });
        assert_eq!(page.read(app).visible_rows(&[], "", false).len(), 13);

        let installed = {
            page.update(app, |page, _| page.filter = Filter::Installed);
            page.read(app).visible_rows(&[], "", false).len()
        };
        assert_eq!(installed, 13);

        assert_eq!(page.read(app).visible_rows(&[], "sftp", false).len(), 1);

        // 权威启停表与 pending 都不应让内置行变成 pending-only 的第二行。
        let sftp = PluginId::parse("kerminal.sftp").unwrap();
        let mut disabled = PluginStoreState::default();
        disabled.builtins.insert(sftp.clone(), false);
        page.update(app, |page, _| page.state = Some(disabled.clone()));
        assert_eq!(
            page.read(app).visible_rows(&[], "sftp", false).len(),
            1,
            "禁用后仍应只有内置行"
        );
        disabled
            .pending
            .push(PendingPluginChange::Enable { id: sftp });
        page.update(app, |page, _| page.state = Some(disabled.clone()));
        assert_eq!(
            page.read(app).visible_rows(&[], "sftp", false).len(),
            1,
            "pending 不能追加重复行"
        );
    });
}

/// ReservedIdentity 记录不能被隐藏，但详情策略禁止启用、卸载与确认。
#[gpui_kit::test]
fn reserved_identity_row_stays_visible_without_actions(app: &mut TestAppContext) {
    let id = PluginId::parse("kerminal.sftp").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(
            PluginSource::Local,
            PluginInstallStatus::Unsupported {
                reason: PluginUnsupportedReason::ReservedIdentity,
            },
        ),
    );
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| page.state = Some(state.clone()));
        // 身份冲突不能因“不受支持”被静默隐藏，必须保留一行供用户识别。
        assert_eq!(
            page.read(app).visible_rows(&[], "", false).len(),
            1,
            "冲突记录必须保持可见"
        );
        // 冲突记录不可启停、确认或重试，只在详情里解释原因。
        assert_eq!(
            stored_actions_policy(&state.plugins[&id], None,),
            StoredActionsPolicy::None
        );
    });
}

/// 同名内置行与冲突记录各自高亮：选内置进启停详情，选记录进冲突详情。
#[gpui_kit::test]
fn same_identity_builtin_and_conflict_rows_select_independently(app: &mut TestAppContext) {
    let units = builtin_units_for_test();
    let id = PluginId::parse("kerminal.sftp").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(
            PluginSource::Local,
            PluginInstallStatus::Unsupported {
                reason: PluginUnsupportedReason::ReservedIdentity,
            },
        ),
    );
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.builtin_units = units.clone();
            page.state = Some(state.clone());
            page.active = true;
        });
        // 两行按不同元素键共存，计数仍按身份只算一个内置/安装身份。
        assert_eq!(page.read(app).visible_rows(&[], "", false).len(), 14);
    });

    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Builtin(id.clone()), cx)
        });
        let page = page.read(app);
        assert_eq!(page.selected, Some(SelectedEntry::Builtin(id.clone())));
        assert!(page.selected_builtin_unit().is_some());
        assert!(page.selected_reserved_record().is_none());
        assert!(page.selected_plugin().is_none());
        assert!(page.row_selected(&SelectedEntry::Builtin(id.clone())));
        assert!(!page.row_selected(&SelectedEntry::Plugin(id.clone())));
    });

    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(id.clone()), cx)
        });
        let page = page.read(app);
        assert_eq!(page.selected, Some(SelectedEntry::Plugin(id.clone())));
        assert!(page.selected_builtin_unit().is_none());
        assert!(page.selected_reserved_record().is_some());
        assert!(page.selected_plugin().is_none());
        assert!(page.row_selected(&SelectedEntry::Plugin(id.clone())));
        assert!(!page.row_selected(&SelectedEntry::Builtin(id.clone())));
        assert_eq!(
            stored_actions_policy(&state.plugins[&id], None),
            StoredActionsPolicy::None
        );
    });
}

/// 同 id 的目录条目不能遮住未确认投放记录；只保留带确认入口的记录行。
#[gpui_kit::test]
fn unconfirmed_drop_in_record_wins_over_same_id_catalog_entry(app: &mut TestAppContext) {
    let drop_in = drop_in_inspection(
        "acme.notes",
        PluginInstallStatus::Ready,
        Some(fingerprint()),
        3,
        false,
    );
    let mut state = PluginStoreState {
        revision: 3,
        ..Default::default()
    };
    state.plugins.insert(
        drop_in.id.clone(),
        installed_record(PluginSource::DropIn, PluginInstallStatus::Ready),
    );
    let store = Arc::new(FakeStore::new(state.clone(), vec![drop_in.clone()]));
    let catalog = Arc::new(FakeCatalog::default());
    let readme_calls = catalog.readme_calls.clone();
    let snapshot = catalog_snapshot(vec![catalog_plugin("acme.notes")]);
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.drop_ins = vec![drop_in.clone()];
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        assert_eq!(
            page.read(app)
                .visible_rows(&snapshot.catalog.plugins, "", false)
                .len(),
            1,
            "目录行与记录行不能同时出现"
        );
        assert!(unconfirmed_drop_in(Some(&state), &drop_in.id).is_some());
        assert_eq!(
            stored_actions_policy(&state.plugins[&drop_in.id], Some(&drop_in)),
            StoredActionsPolicy::ConfirmDropIn
        );
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(drop_in.id.clone()), cx)
        });
        // 目录详情被记录分支替代，确认入口可达且不会为手动投放读取 README。
        assert!(page.read(app).selected_plugin().is_none());
    });
    pump(&runtime, visual);
    assert_eq!(readme_calls.load(Ordering::SeqCst), 0);
}

/// 已确认的 drop-in 与市场目录同 ID 时，详情由目录条目接管，README 仍应经签名目录读取；
/// 只按安装来源提前返回会让可安装条目停在空白详情。
///
/// @author kongweiguang
#[gpui_kit::test]
fn confirmed_drop_in_with_same_id_catalog_entry_requests_readme(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut record = installed_record(PluginSource::DropIn, PluginInstallStatus::Ready);
    record.confirmed = true;
    let mut state = PluginStoreState {
        revision: 3,
        ..Default::default()
    };
    state.plugins.insert(id.clone(), record);
    let catalog = Arc::new(FakeCatalog::default());
    let readme_calls = catalog.readme_calls.clone();
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        catalog,
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        assert!(
            !page
                .read(app)
                .visible_rows(&snapshot.catalog.plugins, "", false)
                .is_empty()
        );
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(id.clone()), cx)
        });
        assert!(
            page.read(app).selected_plugin().is_some(),
            "同 ID 市场条目应接管已确认 drop-in 的详情"
        );
    });
    pump(&runtime, visual);
    assert_eq!(
        readme_calls.load(Ordering::SeqCst),
        1,
        "已解析目录条目必须请求 README"
    );
}

/// drop-in 重读持有 busy 时离开分类要立即释放，重新进入才能正常加载。
#[gpui_kit::test]
fn drop_in_reload_release_on_deactivate_allows_immediate_activate(app: &mut TestAppContext) {
    let drop_in = drop_in_inspection(
        "acme.notes",
        PluginInstallStatus::Ready,
        Some(fingerprint()),
        3,
        false,
    );
    let mut state = PluginStoreState {
        revision: 3,
        ..Default::default()
    };
    state.plugins.insert(
        drop_in.id.clone(),
        installed_record(PluginSource::DropIn, PluginInstallStatus::Ready),
    );
    let store = Arc::new(FakeStore::new(state.clone(), vec![drop_in.clone()]));
    let state_reads = store.state_reads.clone();
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = empty_catalog_snapshot();
    *catalog.refresh_snapshot.lock().unwrap() = Some(snapshot.clone());
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.state = Some(state.clone());
            page.drop_ins = vec![drop_in.clone()];
            page.catalog = Some(snapshot.clone());
            page.active = true;
            page.reload_drop_in_state(cx);
        });
        page.update(app, |page, cx| page.deactivate(cx));
        assert!(!page.read(app).busy, "离开分类必须释放重读 busy");
        page.update(app, |page, cx| page.activate(cx));
        assert!(page.read(app).busy, "重新进入应启动新一次读取");
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy, "读取收尾后页面不能卡在 busy");
        assert!(state_reads.load(Ordering::SeqCst) >= 2);
        assert!(page.state.is_some());
    });
}

/// 工具栏刷新重读磁盘观察，文件修好后无需离开分类即可确认。
#[gpui_kit::test]
fn refresh_reloads_drop_in_observation_for_confirmation(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let missing = drop_in_inspection("acme.notes", PluginInstallStatus::Missing, None, 3, false);
    let mut state = PluginStoreState {
        revision: 3,
        ..Default::default()
    };
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::DropIn, PluginInstallStatus::Ready),
    );
    let store = Arc::new(FakeStore::new(state.clone(), vec![missing.clone()]));
    let inspections = store.inspections.clone();
    let confirm_calls = store.confirm_calls.clone();
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = empty_catalog_snapshot();
    *catalog.refresh_snapshot.lock().unwrap() = Some(snapshot.clone());
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.drop_ins = vec![missing];
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        assert_eq!(
            stored_actions_policy(&state.plugins[&id], page.read(app).drop_ins.first()),
            StoredActionsPolicy::Unavailable
        );
        // 模拟用户修好磁盘文件；工具栏刷新应取得新的指纹，旧观察不能继续阻塞确认。
        *inspections.lock().unwrap() = vec![drop_in_inspection(
            "acme.notes",
            PluginInstallStatus::Ready,
            Some(fingerprint()),
            3,
            false,
        )];
        page.update(app, |page, cx| page.refresh(cx));
    });
    pump(&runtime, visual);
    let operation = visual.update(|_, app| {
        let page = page.read(app);
        let inspection = page.drop_ins.first().expect("刷新后观察存在");
        assert!(drop_in_confirmable(inspection));
        assert_eq!(
            stored_actions_policy(&state.plugins[&id], Some(inspection)),
            StoredActionsPolicy::ConfirmDropIn
        );
        drop_in_confirmation_operation(inspection).expect("修好文件后应能确认")
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| page.run_operation(operation.clone(), cx));
    });
    pump(&runtime, visual);
    assert_eq!(
        confirm_calls.lock().unwrap().as_slice(),
        &[(id.clone(), fingerprint(), 3)]
    );
}

/// 确认失败于修订冲突时不重试旧值，而是提示并重新读取状态与观察。
#[gpui_kit::test]
fn stale_drop_in_confirmation_prompts_and_reloads_without_retry(app: &mut TestAppContext) {
    let drop_in = drop_in_inspection(
        "acme.notes",
        PluginInstallStatus::Ready,
        Some(fingerprint()),
        3,
        false,
    );
    let mut state = PluginStoreState {
        revision: 3,
        ..Default::default()
    };
    state.plugins.insert(
        drop_in.id.clone(),
        installed_record(PluginSource::DropIn, PluginInstallStatus::Ready),
    );
    let store = Arc::new(FakeStore::new(state.clone(), vec![drop_in.clone()]));
    store.conflict_confirmation.store(true, Ordering::SeqCst);
    let state_reads = store.state_reads.clone();
    let inspect_reads = store.inspect_reads.clone();
    let (runtime, page, visual) = page_fixture(app, store, Arc::new(FakeCatalog::default()));
    let operation = drop_in_confirmation_operation(&drop_in).expect("就绪观察应能确认");
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.state = Some(state.clone());
            page.drop_ins = vec![drop_in.clone()];
            page.run_operation(operation.clone(), cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.retry_operation.is_none(), "过期指纹不能保留重试入口");
        assert!(page.status_is_warning);
        assert_eq!(
            page.status.as_deref(),
            Some(
                crate::locale::text(
                    "状态已更改，请刷新",
                    "The state has changed; refresh to continue.",
                )
                .as_ref()
            )
        );
        assert!(state_reads.load(Ordering::SeqCst) >= 1);
        assert!(inspect_reads.load(Ordering::SeqCst) >= 1);
    });
}

/// 确认成功携带观察时的指纹与修订，并回读状态撤下确认入口。
#[gpui_kit::test]
fn confirming_drop_in_uses_frozen_observation(app: &mut TestAppContext) {
    let drop_in = drop_in_inspection(
        "acme.notes",
        PluginInstallStatus::Ready,
        Some(fingerprint()),
        3,
        false,
    );
    let mut state = PluginStoreState {
        revision: 3,
        ..Default::default()
    };
    state.plugins.insert(
        drop_in.id.clone(),
        installed_record(PluginSource::DropIn, PluginInstallStatus::Ready),
    );
    let store = Arc::new(FakeStore::new(state.clone(), vec![drop_in.clone()]));
    let confirm_calls = store.confirm_calls.clone();
    let (runtime, page, visual) = page_fixture(app, store, Arc::new(FakeCatalog::default()));
    let operation = drop_in_confirmation_operation(&drop_in).expect("就绪观察应能确认");
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.state = Some(state.clone());
            page.drop_ins = vec![drop_in.clone()];
            page.run_operation(operation.clone(), cx);
        });
    });
    pump(&runtime, visual);
    assert_eq!(
        confirm_calls.lock().unwrap().as_slice(),
        &[(drop_in.id.clone(), fingerprint(), 3)]
    );
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.status.is_none(), "确认成功不应留下错误提示");
        assert!(page.retry_operation.is_none());
        assert!(
            page.state.as_ref().unwrap().plugins[&drop_in.id].confirmed,
            "确认成功后权威状态必须回读"
        );
        assert!(
            page.drop_ins
                .iter()
                .find(|inspection| inspection.id == drop_in.id)
                .is_some_and(|inspection| inspection.confirmed),
            "观察也要刷新，详情不能继续显示确认入口"
        );
    });
}
