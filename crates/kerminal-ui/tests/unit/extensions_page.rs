//! 扩展页选择只使用当前平台的受信资产和语义版本。
//! @author kongweiguang

use super::render::{ExtensionRow, ListEntry, RowGroup, detail_version_line};
use super::{
    BuiltinRow, CatalogPlugin, Confirmation, DropInState, EmptyStateKind, ExtensionsPage,
    Operation, RetryOperation, SelectedEntry, StoredActionsPolicy, builtin_display_name,
    builtin_name_pair, builtin_rows, cancelable_failed_change, drop_in_confirmable,
    drop_in_confirmation_operation, drop_in_state, empty_state_kind, finish_initial_load,
    installed_total, is_reserved_identity, pending_only_failure, row_entry_indices, selection,
    stored_actions_policy, unconfirmed_drop_in,
};
use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ParentElement as _, Render, Role, Styled as _,
    TestAppContext, VisualTestContext, Window, div, px, size,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    extensions::{
        BuiltinUnit, CatalogIdentity, CatalogSelection, CatalogSnapshot, CatalogSource,
        DEFAULT_PLUGIN_CATALOG_URL, DropInInspection, ExtensionChange, ExtensionReceipt,
        ExtensionsService, InstallProgress, LocalPackageInspection, PluginCatalog,
        PluginPackageStore,
    },
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginFingerprint, PluginId, PluginInstallStatus,
    PluginSource, PluginStoreState, PluginUnsupportedReason, PluginVersion,
};
use kerminal_plugin_protocol::Platform;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::{oneshot, watch},
};
use tokio_util::sync::CancellationToken;

/// 测试目录身份只用于把选择绑定到固定快照；摘要内容不参与界面逻辑。
fn test_identity() -> CatalogIdentity {
    CatalogIdentity::new("test", 0, "a".repeat(64))
}

/// 行不再携带组字段，分组只由条目序列中的标题决定；测试按标题归属还原分组，
/// 避免另存一份会与渲染索引漂移的分组逻辑。
fn rows_in_group(entries: &[ListEntry], group: RowGroup) -> Vec<&ExtensionRow> {
    let mut current = None;
    let mut rows = Vec::new();
    for entry in entries {
        match entry {
            ListEntry::Header { group: header, .. } => current = Some(*header),
            ListEntry::Row(row) if current == Some(group) => rows.push(row.as_ref()),
            ListEntry::Row(_) => {}
        }
    }
    rows
}

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
    let selected = selection(&entry, "1.0.0", Some(&test_identity())).unwrap();
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
    assert!(selection(&entry, "1.0.0", Some(&test_identity())).is_none());
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
    let rows: Vec<BuiltinRow> = builtin_rows(&units, Some(&enabled), "");
    assert_eq!(rows.len(), 13);
    assert_eq!(builtin_rows(&units, Some(&enabled), "").len(), 13);

    // 搜索同时覆盖稳定身份与当前语言展示名，界面看到什么就能搜到什么。
    let by_id = builtin_rows(&units, Some(&enabled), "sftp");
    assert_eq!(by_id.len(), 1);
    assert_eq!(by_id[0].id, sftp);
    let ports = units
        .iter()
        .find(|unit| unit.id.as_str() == "kerminal.ports")
        .expect("端口转发单元存在");
    let ports_name = builtin_display_name(ports);
    let by_name = builtin_rows(&units, Some(&enabled), &ports_name.to_lowercase());
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
    let row = builtin_rows(&units, Some(&disabled), "kerminal.sftp");
    assert!(!row[0].enabled);
    assert!(!row[0].pending);

    // 排队启停后必须显示重启生效，而不是把旧启停状态当成最终结果。
    disabled
        .pending
        .push(PendingPluginChange::Enable { id: sftp.clone() });
    let row = builtin_rows(&units, Some(&disabled), "kerminal.sftp");
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

/// 统一列表顺序固定为已安装→内置→待重启→可安装，同一身份只出现一次；
/// 内置收起时仍保留标题，搜索展开不改变用户的折叠选择。
#[gpui_kit::test]
fn unified_list_orders_groups_and_dedupes_identities(app: &mut TestAppContext) {
    let installed = PluginId::parse("acme.installed").unwrap();
    let pending = PluginId::parse("acme.pending").unwrap();
    let market = PluginId::parse("acme.market").unwrap();
    let builtin = PluginId::parse("kerminal.sftp").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        installed.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    state.pending.push(PendingPluginChange::Install {
        id: pending.clone(),
        to: PluginVersion::parse("1.0.0").unwrap(),
        staged: "staged".into(),
        fingerprint: None,
        source: PluginSource::Local,
        installed_at: "2026-01-01".into(),
    });
    let snapshot = catalog_snapshot(vec![
        catalog_plugin(installed.as_str()),
        catalog_plugin(pending.as_str()),
        catalog_plugin(market.as_str()),
    ]);
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.builtin_units = vec![BuiltinUnit {
                id: builtin.clone(),
                name: "SFTP".to_owned(),
                can_disable: true,
            }];
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.builtin_expanded = false;
            page.active = true;
        });
        let entries = page.read(app).list_entries(&snapshot.catalog.plugins, "");
        let headers: Vec<(RowGroup, usize)> = entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Header { group, count } => Some((*group, *count)),
                ListEntry::Row(_) => None,
            })
            .collect();
        assert_eq!(
            headers,
            vec![
                (RowGroup::External, 1),
                (RowGroup::Builtin, 1),
                (RowGroup::Pending, 1),
                (RowGroup::Market, 1),
            ],
            "组顺序与计数必须来自同一份条目序列"
        );
        let count_selection = |selection: &SelectedEntry| {
            entries
                .iter()
                .filter(|entry| matches!(entry, ListEntry::Row(row) if &row.selection == selection))
                .count()
        };
        assert_eq!(
            count_selection(&SelectedEntry::Plugin(installed.clone())),
            1,
            "已安装身份不能在目录组重复"
        );
        assert_eq!(
            count_selection(&SelectedEntry::Plugin(pending.clone())),
            1,
            "待重启身份不能在目录组重复"
        );
        assert!(
            rows_in_group(&entries, RowGroup::Builtin).is_empty(),
            "默认收起时内置组只保留标题"
        );
        let market_rows = rows_in_group(&entries, RowGroup::Market);
        assert_eq!(market_rows.len(), 1);
        assert_eq!(market_rows[0].element_key, market.as_str());

        // 搜索只临时展开内置命中，不写回 builtin_expanded。
        let searched = page
            .read(app)
            .list_entries(&snapshot.catalog.plugins, "sftp");
        assert!(
            !rows_in_group(&searched, RowGroup::Builtin).is_empty(),
            "搜索命中必须临时展开内置组"
        );
        assert!(
            !page.read(app).builtin_expanded,
            "搜索不能改写用户的折叠选择"
        );
    });
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
    /// 激活读取的缓存快照；用于证明目录成功而状态失败时市场仍可浏览。
    cached_snapshot: Arc<Mutex<Option<CatalogSnapshot>>>,
    /// 目录缓存读取失败与状态读取失败必须能独立注入，证明两者不会互相阻断。
    fail_cached: Arc<AtomicBool>,
    /// 一次性闸门让测试在读取进行中离开分类，验证迟到回执不会写回页面。
    cached_gate: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    /// 一次性闸门让测试冻结 README 请求，验证切换选择后旧回执按代次丢弃。
    readme_gate: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    /// 记录每次 README 请求的取消令牌，离开分类后可直接断言取消已传播。
    readme_tokens: Arc<Mutex<Vec<CancellationToken>>>,
    /// 网络刷新闸门用于证明目录刷新不再占用写忙态，本地管理可在刷新期间继续。
    refresh_gate: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    refresh_calls: Arc<AtomicUsize>,
    /// 当前生效来源；空串表示官方默认，与设置快照的存储语义一致。
    ///
    /// 测试目录必须真实保存切换结果：页面只应在设置保存成功后才调用
    /// `set_source_url`，返回无效果的成功会让失败用例假通过。
    source_url: Arc<Mutex<String>>,
}

#[async_trait]
impl CatalogSource for FakeCatalog {
    /// 官方默认以空串存储、以真实常量回显；页面据此把输入回填为空。
    fn source_url(&self) -> String {
        let url = self.source_url.lock().unwrap().clone();
        if url.is_empty() {
            DEFAULT_PLUGIN_CATALOG_URL.to_owned()
        } else {
            url
        }
    }

    /// 测试目录必须真实反映来源切换；非法地址显式失败，不能返回无效果的成功。
    fn set_source_url(&self, source_url: &str) -> AppResult<()> {
        let trimmed = source_url.trim();
        if !trimmed.is_empty() {
            // 公开错误文案只能来自源码常量，测试也不把动态输入拼进错误。
            kerminal_plugin_protocol::validate_asset_url(trimmed)
                .map_err(|_| AppError::invalid("测试目录地址无效"))?;
        }
        *self.source_url.lock().unwrap() = trimmed.to_owned();
        Ok(())
    }

    /// 测试页面直接注入目录快照；缓存读取保持为空，避免触发真实 I/O。
    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>> {
        let gate = self.cached_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        if self.fail_cached.load(Ordering::SeqCst) {
            return Err(AppError::infrastructure("测试目录缓存损坏"));
        }
        Ok(self.cached_snapshot.lock().unwrap().clone())
    }

    /// 计数 README 请求是断言“内置与 drop-in 不发网络”的唯一证据；返回带 ID 的
    /// 文本，让切换选择后能辨认最终落地的是哪一次请求。
    async fn readme(
        &self,
        id: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        self.readme_calls.fetch_add(1, Ordering::SeqCst);
        self.readme_tokens
            .lock()
            .unwrap()
            .push(cancellation.clone());
        let gate = self.readme_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        Ok(Some(format!("readme:{}", id.as_str())))
    }

    /// 只有注入快照的用例走成功刷新；默认失败好过伪造一份可安装目录。
    async fn refresh(&self, _cancellation: CancellationToken) -> AppResult<CatalogSnapshot> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.refresh_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        self.refresh_snapshot
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::invalid("测试不请求市场目录"))
    }
}

/// 目录安装的可编排终态：批量测试按调用顺序制造成功、失败、未知、取消与清理告警，
/// 从而验证串行边界，而不模拟真实磁盘或签名目录。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FakeInstallOutcome {
    Success,
    SuccessWithCleanupWarning,
    Failure,
    Unknown,
    Cancelled,
}

/// 手动投放的只读状态与确认回执都保存在内存，测试可观察每次调用参数与读回次数。
struct FakeStore {
    state: Arc<Mutex<PluginStoreState>>,
    inspections: Arc<Mutex<Vec<DropInInspection>>>,
    state_reads: Arc<AtomicUsize>,
    inspect_reads: Arc<AtomicUsize>,
    confirm_calls: Arc<Mutex<Vec<(PluginId, String, u64)>>>,
    conflict_confirmation: Arc<AtomicBool>,
    /// 状态读取失败独立注入；市场目录仍应可浏览。
    fail_state: Arc<AtomicBool>,
    /// 一次性闸门让测试冻结状态读取，证明目录结果可以先独立落地。
    state_gate: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    /// 文件预检闸门用于验证离开分类后旧结果不能弹出确认或释放新 busy。
    inspect_gate: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    inspect_calls: Arc<AtomicUsize>,
    change_calls: Arc<AtomicUsize>,
    /// 写入成功后让下一次状态回读失败，覆盖“已提交但回读失败”。
    fail_state_after_change: Arc<AtomicBool>,
    /// 写入本身返回未知提交，覆盖“先回读再决定是否重试”。
    change_outcome_unknown: Arc<AtomicBool>,
    /// 目录安装按调用顺序消费的脚本结果；队列耗尽后回落到失败，避免测试静默成功。
    install_outcomes: Arc<Mutex<VecDeque<FakeInstallOutcome>>>,
    /// 记录每次目录安装的身份，证明失败或取消后不再启动后续项。
    install_calls: Arc<Mutex<Vec<PluginId>>>,
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
            fail_state: Arc::new(AtomicBool::new(false)),
            state_gate: Arc::new(Mutex::new(None)),
            inspect_gate: Arc::new(Mutex::new(None)),
            inspect_calls: Arc::new(AtomicUsize::new(0)),
            change_calls: Arc::new(AtomicUsize::new(0)),
            fail_state_after_change: Arc::new(AtomicBool::new(false)),
            change_outcome_unknown: Arc::new(AtomicBool::new(false)),
            install_outcomes: Arc::new(Mutex::new(VecDeque::new())),
            install_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl PluginPackageStore for FakeStore {
    /// 每次读取都计数，用于证明冲突后确实重新回读了权威状态。
    async fn state(&self) -> AppResult<PluginStoreState> {
        self.state_reads.fetch_add(1, Ordering::SeqCst);
        let gate = self.state_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        if self.fail_state.load(Ordering::SeqCst) {
            return Err(AppError::infrastructure("测试状态仓储不可读"));
        }
        Ok(self.state.lock().unwrap().clone())
    }

    /// 目录安装按脚本顺序返回终态；成功时同步推进记录与 pending，让批量路径的
    /// 权威回读和“已提交保留 pending”断言都有真实数据可读。
    async fn install_catalog(
        &self,
        selection: CatalogSelection,
        _progress: watch::Sender<InstallProgress>,
        _cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        self.install_calls
            .lock()
            .unwrap()
            .push(selection.id.clone());
        let outcome = self
            .install_outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(FakeInstallOutcome::Failure);
        match outcome {
            FakeInstallOutcome::Failure => Err(AppError::invalid("测试安装失败")),
            FakeInstallOutcome::Unknown => Err(AppError::outcome_unknown("测试安装结果未知")),
            FakeInstallOutcome::Cancelled => Err(AppError::cancelled("测试安装已取消")),
            FakeInstallOutcome::Success | FakeInstallOutcome::SuccessWithCleanupWarning => {
                let mut state = self.state.lock().unwrap();
                let mut record = state
                    .plugins
                    .get(&selection.id)
                    .cloned()
                    .unwrap_or_else(|| {
                        installed_record(PluginSource::Official, PluginInstallStatus::Ready)
                    });
                record.active = selection.version.clone();
                record.status = PluginInstallStatus::Ready;
                record.fingerprint = Some(PluginFingerprint::parse(fingerprint()).unwrap());
                state.plugins.insert(selection.id.clone(), record);
                state.pending.push(PendingPluginChange::Update {
                    id: selection.id.clone(),
                    to: selection.version.clone(),
                    staged: format!("staged-{}", selection.id.as_str()),
                    fingerprint: None,
                });
                state.revision += 1;
                let revision = state.revision;
                drop(state);
                // 每项提交后让下一次回读失败，覆盖“已提交但回读失败”的停止边界。
                if self.fail_state_after_change.swap(false, Ordering::SeqCst) {
                    self.fail_state.store(true, Ordering::SeqCst);
                }
                Ok(ExtensionReceipt {
                    revision,
                    restart_required: true,
                    cleanup_warning: (outcome == FakeInstallOutcome::SuccessWithCleanupWarning)
                        .then(|| AppError::infrastructure("测试残留未清理")),
                })
            }
        }
    }

    async fn inspect_file(
        &self,
        _path: PathBuf,
        _cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection> {
        self.inspect_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.inspect_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        Ok(LocalPackageInspection {
            id: PluginId::parse("acme.local").unwrap(),
            version: PluginVersion::parse("1.0.0").unwrap(),
            name: Default::default(),
            permissions: Vec::new(),
            size: 1,
            sha256: "a".repeat(64),
        })
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
        self.change_calls.fetch_add(1, Ordering::SeqCst);
        if self.change_outcome_unknown.load(Ordering::SeqCst) {
            return Err(AppError::outcome_unknown("测试未知提交"));
        }
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
        // 模拟“提交成功、随后回读失败”：下一次 state() 返回错误，页面必须停写并回读。
        if self.fail_state_after_change.swap(false, Ordering::SeqCst) {
            self.fail_state.store(true, Ordering::SeqCst);
        }
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
        identity: test_identity(),
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
        identity: test_identity(),
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

/// 读取当前窗口中可见菜单项的标签；入口断言按文案而不是菜单顺序耦合。
///
/// @author kongweiguang
fn visible_menu_labels(visual: &mut VisualTestContext) -> Vec<String> {
    visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.visible() && node.role() == Some(Role::MenuItem))
            .filter_map(|node| node.label().map(ToString::to_string))
            .collect()
    })
}

/// 按可见标签点击菜单项；菜单内容在打开后才生成，所以必须先取真实快照。
///
/// @author kongweiguang
fn click_menu_item(visual: &mut VisualTestContext, label: &str) {
    let center = visual
        .update(|window, _| {
            gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.visible()
                        && node.role() == Some(Role::MenuItem)
                        && node.label().is_some_and(|value| value == label)
                })
                .map(|node| node.bounds().center())
        })
        .unwrap_or_else(|| panic!("菜单必须包含 {label}"));
    visual.simulate_click(center, Modifiers::default());
}

/// Button 的键盘激活依赖同一焦点代次内的 KeyDown 与 KeyUp；`simulate_keystrokes`
/// 只派发 KeyDown，无法覆盖行内按钮真实的 Enter 激活路径。
fn press_enter(visual: &mut VisualTestContext) {
    let keystroke = Keystroke::parse("enter").expect("enter 键可解析");
    visual.simulate_event(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    visual.simulate_event(KeyUpEvent { keystroke });
}

/// 外层探针只记录页面没有消费而冒泡上来的 Escape；不创建第二个运行时。
struct EscapeProbe {
    page: Entity<ExtensionsPage>,
    escapes: Rc<Cell<usize>>,
}

impl Render for EscapeProbe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| {
                if event.keystroke.key.eq_ignore_ascii_case("escape") {
                    this.escapes.set(this.escapes.get() + 1);
                }
            }))
            .child(self.page.clone())
    }
}

/// 与 page_fixture 相同的页面装配，但把页面包在外层按键探针里。
fn probe_fixture(
    app: &mut TestAppContext,
    store: Arc<FakeStore>,
    catalog: Arc<FakeCatalog>,
) -> (
    Runtime,
    Entity<ExtensionsPage>,
    Rc<Cell<usize>>,
    &mut VisualTestContext,
) {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试运行时");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let service = Arc::new(ExtensionsService::new(catalog, store));
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let escapes = Rc::new(Cell::new(0));
    let escapes_out = escapes.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let page =
            cx.new(|cx| ExtensionsPage::new(service, executor, "1.0.0".to_owned(), window, cx));
        *mounted.borrow_mut() = Some(page.clone());
        EscapeProbe { page, escapes }
    });
    visual.simulate_resize(size(px(900.), px(600.)));
    let page = slot.borrow_mut().take().expect("扩展页已挂载");
    (runtime, page, escapes_out, visual)
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
            page.builtin_expanded = true;
        });
        assert_eq!(page.read(app).visible_rows(&[], "").len(), 13);

        let installed = page.read(app).visible_rows(&[], "").len();
        assert_eq!(installed, 13);

        assert_eq!(page.read(app).visible_rows(&[], "sftp").len(), 1);

        // 权威启停表与 pending 都不应让内置行变成 pending-only 的第二行。
        let sftp = PluginId::parse("kerminal.sftp").unwrap();
        let mut disabled = PluginStoreState::default();
        disabled.builtins.insert(sftp.clone(), false);
        page.update(app, |page, _| page.state = Some(disabled.clone()));
        assert_eq!(
            page.read(app).visible_rows(&[], "sftp").len(),
            1,
            "禁用后仍应只有内置行"
        );
        disabled
            .pending
            .push(PendingPluginChange::Enable { id: sftp });
        page.update(app, |page, _| page.state = Some(disabled.clone()));
        assert_eq!(
            page.read(app).visible_rows(&[], "sftp").len(),
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
        page.update(app, |page, _| {
            page.state = Some(state.clone());
        });
        // 身份冲突不能因“不受支持”被静默隐藏，必须保留一行供用户识别。
        assert_eq!(
            page.read(app).visible_rows(&[], "").len(),
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
            page.builtin_expanded = true;
        });
        // 两行按不同元素键共存，计数仍按身份只算一个内置/安装身份。
        assert_eq!(page.read(app).visible_rows(&[], "").len(), 14);
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
        // 统一列表没有市场/已安装模式：同 id 目录行必须被记录行去重，
        // 只保留带确认入口的记录行，不能出现两行同身份。
        let rows = page.read(app).visible_rows(&snapshot.catalog.plugins, "");
        assert_eq!(rows.len(), 1, "目录行与记录行不能同时出现");
        assert_eq!(rows[0].element_key, drop_in.id.as_str());
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
                .visible_rows(&snapshot.catalog.plugins, "")
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

/// 目录缓存失败只记录目录错误；已安装记录与内置单元仍必须可管理。
#[gpui_kit::test]
fn catalog_failure_does_not_block_installed_state(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let store = Arc::new(FakeStore::new(state, Vec::new()));
    let catalog = Arc::new(FakeCatalog::default());
    catalog.fail_cached.store(true, Ordering::SeqCst);
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.builtin_expanded = true;
        });
        page.update(app, |page, cx| page.activate(cx));
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.state.is_some(), "状态读取不能被目录失败阻断");
        assert!(page.catalog_error.is_some(), "目录失败必须单独记录");
        assert!(page.state_error.is_none());
        assert!(!page.busy);
        let rows = page.visible_rows(&[], "");
        assert!(
            rows.iter()
                .any(|row| row.selection == SelectedEntry::Plugin(id.clone())),
            "已安装记录必须保持可见"
        );
        assert_eq!(rows.len(), 14, "13 个内置单元与 1 个已安装记录");
    });
}

/// 权威状态失败只记录状态错误；已验签目录仍可浏览，但写入动作由状态门禁禁用。
#[gpui_kit::test]
fn state_failure_does_not_block_catalog_browsing(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    store.fail_state.store(true, Ordering::SeqCst);
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    *catalog.cached_snapshot.lock().unwrap() = Some(snapshot.clone());
    // 缓存时间戳为 0 会触发进入时的按需刷新；让刷新也返回同一份已验证快照，
    // 才能证明目录路径本身健康，catalog_error 为空不是被状态失败掩盖。
    *catalog.refresh_snapshot.lock().unwrap() = Some(snapshot.clone());
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, cx| page.activate(cx));
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.catalog.is_some(), "目录读取不能被状态失败阻断");
        assert!(page.state_error.is_some(), "状态失败必须单独记录");
        assert!(page.catalog_error.is_none());
        assert!(!page.busy);
        assert!(
            page.visible_rows(&snapshot.catalog.plugins, "")
                .iter()
                .any(|row| row.selection == SelectedEntry::Plugin(id.clone())),
            "目录条目必须保持可见"
        );
    });
}

/// 目录与状态各自独立落地：状态读取被冻结时，已验签目录仍应立即出现在页面。
///
/// @author kongweiguang
#[gpui_kit::test]
fn catalog_result_lands_before_state_finishes(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    let (gate_tx, gate_rx) = oneshot::channel();
    *store.state_gate.lock().unwrap() = Some(gate_rx);
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    *catalog.cached_snapshot.lock().unwrap() = Some(snapshot.clone());
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, cx| page.activate(cx));
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.catalog.is_some(), "目录结果必须先独立落地");
        assert!(page.state.is_none(), "状态读取仍被冻结");
        assert!(page.busy, "任一侧未完成时页面仍应保持忙态");
        assert!(
            page.visible_rows(&snapshot.catalog.plugins, "")
                .iter()
                .any(|row| row.selection == SelectedEntry::Plugin(id.clone())),
            "目录条目无需等待状态即可浏览"
        );
    });
    let _ = gate_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.state.is_some(), "放行后状态结果落地");
        assert!(!page.busy, "两侧都结束后释放忙态");
    });
}

/// 离开分类会推进代次并取消读取；迟到的目录回执不能写回已离开的页面。
#[gpui_kit::test]
fn leaving_section_discards_late_catalog_result(app: &mut TestAppContext) {
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    let catalog = Arc::new(FakeCatalog::default());
    let (gate_tx, gate_rx) = oneshot::channel();
    *catalog.cached_gate.lock().unwrap() = Some(gate_rx);
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, cx| page.activate(cx));
        page.update(app, |page, cx| page.deactivate(cx));
        assert!(!page.read(app).busy, "离开分类必须释放只读加载忙态");
    });
    let _ = gate_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.catalog.is_none(), "迟到目录回执不能写回页面");
        assert!(page.catalog_error.is_none(), "取消不能被写成错误");
        assert!(!page.busy);
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

/// 空态必须把状态未知、读取中、搜索无匹配、分类无匹配、目录失败和真实空列表
/// 分开；统一列表没有“市场/已安装”模式，优先级由同一函数固定。
#[test]
fn empty_state_distinguishes_failure_filter_and_real_empty() {
    assert_eq!(
        empty_state_kind("", None, true, true, true, true),
        EmptyStateKind::StateUnknown
    );
    assert_eq!(
        empty_state_kind("", None, true, false, false, false),
        EmptyStateKind::StateReading
    );
    assert_eq!(
        empty_state_kind("x", None, true, true, true, false),
        EmptyStateKind::SearchNoMatch
    );
    assert_eq!(
        empty_state_kind("", None, false, true, true, false),
        EmptyStateKind::CatalogUnavailable
    );
    assert_eq!(
        empty_state_kind("", None, false, false, true, false),
        EmptyStateKind::CatalogReading
    );
    assert_eq!(
        empty_state_kind("", Some("dev"), true, false, true, false),
        EmptyStateKind::CategoryEmpty("dev".to_owned())
    );
    assert_eq!(
        empty_state_kind("", None, true, false, true, false),
        EmptyStateKind::Empty
    );
}

/// 状态未知时页头只说明“未确认”，不能用静态内置数量冒充已安装总数。
#[gpui_kit::test]
fn header_status_marks_unknown_state(app: &mut TestAppContext) {
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(PluginStoreState::default());
            page.state_error = Some("状态不可读".to_owned());
        });
        assert_eq!(
            page.read(app).header_status(),
            Some(crate::locale::text(
                "已安装状态未确认",
                "Installed state unconfirmed"
            ))
        );
        page.update(app, |page, _| page.state_error = None);
        assert!(page.read(app).header_status().is_none());
    });
}

/// 首次安装待重启只保留一条提交事实，不再同时出现“未安装/可用版本/重启后生效”。
#[test]
fn pending_first_install_status_is_single_submission_fact() {
    let plugin = catalog_plugin("acme.market-two");
    let available = selection(&plugin, "1.0.0", Some(&test_identity())).expect("可安装版本");
    let install = PendingPluginChange::Install {
        id: plugin.id.clone(),
        to: PluginVersion::parse("1.0.0").unwrap(),
        staged: "staged-install".into(),
        fingerprint: None,
        source: PluginSource::Official,
        installed_at: "2026-01-01T00:00:00Z".into(),
    };
    let first = detail_version_line(Some(&available), None, Some(&install), false);
    assert!(
        first.contains("安装已提交") || first.contains("Installation submitted"),
        "{first}"
    );
    assert!(
        first.contains("重启后生效") || first.contains("Restart required"),
        "{first}"
    );
    assert!(
        !first.contains("未安装") && !first.contains("Not installed"),
        "{first}"
    );
    assert!(
        !first.contains("v1.0.0"),
        "首次安装待重启不再重复版本: {first}"
    );

    let mut installed = installed_record(PluginSource::Official, PluginInstallStatus::Ready);
    installed.active = PluginVersion::parse("0.9.0").unwrap();
    let update = PendingPluginChange::Update {
        id: plugin.id.clone(),
        to: PluginVersion::parse("1.1.0").unwrap(),
        staged: "staged-update".into(),
        fingerprint: None,
    };
    let update = detail_version_line(Some(&available), Some(&installed), Some(&update), false);
    assert!(update.contains("0.9.0"), "{update}");
    assert!(update.contains("1.1.0"), "{update}");
    assert!(
        !update.contains("v1.0.0"),
        "更新目标必须取 pending 记录而不是目录版本: {update}"
    );
    assert!(
        !update.contains("可用") && !update.contains("Available"),
        "更新待重启不再重复可用版本口径: {update}"
    );
}

/// 待重启文案跟随真实变更类型；同版本启停/卸载不能被投影成“更新至”。
#[test]
fn pending_version_line_distinguishes_change_kind() {
    let plugin = catalog_plugin("acme.market-two");
    let available = selection(&plugin, "1.0.0", Some(&test_identity())).expect("可安装版本");
    let mut installed = installed_record(PluginSource::Official, PluginInstallStatus::Ready);
    installed.active = PluginVersion::parse("0.9.0").unwrap();

    let update = PendingPluginChange::Update {
        id: plugin.id.clone(),
        to: PluginVersion::parse("1.1.0").unwrap(),
        staged: "staged-update".into(),
        fingerprint: None,
    };
    let update_line = detail_version_line(Some(&available), Some(&installed), Some(&update), false);
    assert!(
        update_line.contains("当前 v0.9.0") || update_line.contains("Current v0.9.0"),
        "{update_line}"
    );
    assert!(
        update_line.contains("更新至 v1.1.0") || update_line.contains("Update to v1.1.0"),
        "{update_line}"
    );
    assert!(!update_line.contains("v1.0.0"), "{update_line}");
    assert!(
        !update_line.contains("安装已提交") && !update_line.contains("Installation submitted"),
        "{update_line}"
    );

    installed.active = PluginVersion::parse("1.0.0").unwrap();
    for change in [
        PendingPluginChange::Disable {
            id: plugin.id.clone(),
        },
        PendingPluginChange::Enable {
            id: plugin.id.clone(),
        },
        PendingPluginChange::Uninstall {
            id: plugin.id.clone(),
            operation_id: String::new(),
        },
    ] {
        let line = detail_version_line(Some(&available), Some(&installed), Some(&change), false);
        assert!(
            line.contains("变更已提交") || line.contains("Change submitted"),
            "{line}"
        );
        assert!(
            !line.contains("更新至") && !line.contains("Update to"),
            "{line}"
        );
        assert!(
            !line.contains("安装已提交") && !line.contains("Installation submitted"),
            "{line}"
        );
    }
}

/// 启动应用 pending 失败时版本行必须陈述失败；只有 pending_failures 绑定才成立，
/// 遗留无绑定 failure 不能误配后来排队的新变更。
#[test]
fn pending_version_line_prefers_bound_failure_and_ignores_legacy_failure() {
    let plugin = catalog_plugin("acme.market-two");
    let available = selection(&plugin, "1.0.0", Some(&test_identity())).expect("可安装版本");
    let mut installed = installed_record(PluginSource::Official, PluginInstallStatus::Ready);
    installed.active = PluginVersion::parse("0.9.0").unwrap();
    let update = PendingPluginChange::Update {
        id: plugin.id.clone(),
        to: PluginVersion::parse("1.1.0").unwrap(),
        staged: "staged-update".into(),
        fingerprint: None,
    };

    let mut state = PluginStoreState::default();
    state.plugins.insert(plugin.id.clone(), installed.clone());
    state.pending.push(update.clone());
    state
        .failures
        .insert(plugin.id.clone(), "插件启动失败".into());

    // 只有 failures 没有 pending_failures 时属于遗留失败，不能借用给新 pending。
    assert!(cancelable_failed_change(&state, &plugin.id).is_none());
    let normal = detail_version_line(
        Some(&available),
        Some(&installed),
        Some(&update),
        cancelable_failed_change(&state, &plugin.id).is_some(),
    );
    assert!(
        normal.contains("更新至 v1.1.0") || normal.contains("Update to v1.1.0"),
        "{normal}"
    );
    assert!(
        !normal.contains("变更失败") && !normal.contains("Change failed"),
        "{normal}"
    );

    // 绑定后版本行优先陈述失败与当前版本，不能再声称已提交待生效。
    state.pending_failures.insert(plugin.id.clone());
    assert!(cancelable_failed_change(&state, &plugin.id).is_some());
    let failed = detail_version_line(
        Some(&available),
        Some(&installed),
        Some(&update),
        cancelable_failed_change(&state, &plugin.id).is_some(),
    );
    assert!(
        failed.contains("当前 v0.9.0") || failed.contains("Current v0.9.0"),
        "{failed}"
    );
    assert!(
        failed.contains("变更失败") || failed.contains("Change failed"),
        "{failed}"
    );
    assert!(
        !failed.contains("已提交") && !failed.contains("submitted"),
        "{failed}"
    );
    assert!(
        !failed.contains("重启后生效") && !failed.contains("Restart required"),
        "{failed}"
    );

    for change in [
        PendingPluginChange::Disable {
            id: plugin.id.clone(),
        },
        PendingPluginChange::Uninstall {
            id: plugin.id.clone(),
            operation_id: String::new(),
        },
    ] {
        let line = detail_version_line(Some(&available), Some(&installed), Some(&change), true);
        assert!(
            line.contains("变更失败") || line.contains("Change failed"),
            "{line}"
        );
        assert!(
            !line.contains("更新至") && !line.contains("Update to"),
            "{line}"
        );
        assert!(
            !line.contains("已提交") && !line.contains("submitted"),
            "{line}"
        );
    }
}

/// 详情版本行必须使用真实 pending 失败绑定；无绑定 failure 不能切换失败投影。
#[gpui_kit::test]
fn detail_version_line_uses_bound_failure_projection(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.failure").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    let mut installed = installed_record(PluginSource::Official, PluginInstallStatus::Ready);
    installed.active = PluginVersion::parse("0.9.0").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(id.clone(), installed);
    state.pending.push(PendingPluginChange::Update {
        id: id.clone(),
        to: PluginVersion::parse("1.1.0").unwrap(),
        staged: "staged-update".into(),
        fingerprint: None,
    });
    state.failures.insert(id.clone(), "插件启动失败".into());
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(state.clone());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(id.clone()), cx)
        });
    });
    pump(&runtime, visual);
    assert!(
        visual.debug_bounds("extension-detail-version").is_some(),
        "无绑定 failure 应保持正常版本行"
    );
    assert!(
        visual
            .debug_bounds("extension-detail-version-failed")
            .is_none(),
        "遗留 failure 不能误报为当前 pending 失败"
    );

    visual.update(|_, app| {
        page.update(app, |page, _| {
            if let Some(state) = page.state.as_mut() {
                state.pending_failures.insert(id.clone());
            }
        });
    });
    pump(&runtime, visual);
    assert!(
        visual
            .debug_bounds("extension-detail-version-failed")
            .is_some(),
        "绑定失败后版本行应切换为失败投影"
    );
    assert!(
        visual.debug_bounds("extension-detail-version").is_none(),
        "失败投影不能同时保留正常版本行"
    );
}

#[gpui_kit::test]
fn detail_install_action_is_compact_and_left_aligned(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.aa").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(id.clone()), cx)
        });
    });
    pump(&runtime, visual);
    let install = visual
        .debug_bounds("extension-install")
        .expect("安装按钮可见");
    let repository = visual
        .debug_bounds("extension-repository")
        .expect("仓库链接可见");
    let actions = visual
        .debug_bounds("extension-detail-actions")
        .expect("动作栏可见");
    assert!(
        install.size.width < px(240.),
        "主按钮不应撑满详情栏: {:?}",
        install.size
    );
    assert_eq!(
        install.origin.x, actions.origin.x,
        "主按钮应贴在动作栏左边缘，而不是被居中或拉满"
    );
    assert!(
        repository.origin.x >= install.origin.x
            && repository.origin.x < actions.origin.x + actions.size.width,
        "仓库链接应留在同一动作栏内: install={:?} repository={:?} actions={:?}",
        install.origin,
        repository.origin,
        actions
    );
}

/// 商店来源和从文件安装在所有宽度都收在同一更多菜单；宽窄切换不改变入口
/// 形态，也不把低频动作平铺回页头挤占刷新与重启。
#[gpui_kit::test]
fn header_more_menu_lists_source_and_file_at_all_widths(app: &mut TestAppContext) {
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
    });
    pump(&runtime, visual);
    let source = crate::locale::text("商店来源…", "Store source…");
    let file = crate::locale::text("从文件安装…", "Install from file…");
    for stage in ["宽窗", "窄窗"] {
        if stage == "窄窗" {
            visual.simulate_resize(size(px(400.), px(600.)));
            pump(&runtime, visual);
        }
        let more = visual
            .debug_bounds("extensions-more")
            .unwrap_or_else(|| panic!("{stage}必须保留更多菜单入口"));
        assert!(
            visual.debug_bounds("extensions-source").is_none(),
            "{stage}不应再平铺商店来源文字按钮"
        );
        visual.simulate_click(more.center(), Modifiers::default());
        pump(&runtime, visual);
        let labels = visible_menu_labels(visual);
        assert!(
            labels.iter().any(|label| label == source.as_ref()),
            "{stage}更多菜单必须包含商店来源: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label == file.as_ref()),
            "{stage}更多菜单必须包含从文件安装: {labels:?}"
        );
        // 关闭菜单后进入下一宽度；Escape 由 Kit 顶层菜单负责。
        visual.simulate_keystrokes("escape");
        pump(&runtime, visual);
    }
}

/// 商店来源入口在宽窄两种宽度都必须经由更多菜单真实打开编辑器，不能只验证
/// 菜单文案存在；菜单项顺序无关，按文案定位。
#[gpui_kit::test]
fn catalog_source_menu_entry_opens_editor_at_all_widths(app: &mut TestAppContext) {
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
    });
    pump(&runtime, visual);
    let label = crate::locale::text("商店来源…", "Store source…");
    for stage in ["宽窗", "窄窗"] {
        if stage == "窄窗" {
            visual.simulate_resize(size(px(400.), px(600.)));
            pump(&runtime, visual);
        }
        let more = visual
            .debug_bounds("extensions-more")
            .unwrap_or_else(|| panic!("{stage}更多入口可见"));
        visual.simulate_click(more.center(), Modifiers::default());
        pump(&runtime, visual);
        click_menu_item(visual, label.as_ref());
        pump(&runtime, visual);
        assert!(
            visual.update(|_, app| page.read(app).source_dialog_open),
            "{stage}菜单项必须能打开来源编辑"
        );
        visual.update(|_, app| {
            page.update(app, |page, cx| page.close_source_dialog(cx));
        });
        pump(&runtime, visual);
    }
}

/// 删除工具栏独立感叹号后，状态与安装错误仍必须保留原因和恢复入口：入口移到
/// 页头副标题，点击展开与之前相同的重试/刷新按钮。
#[gpui_kit::test]
fn state_error_recovery_lives_in_header_instead_of_toolbar(app: &mut TestAppContext) {
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.state_error = Some("状态不可读".to_owned());
            page.active = true;
        });
    });
    pump(&runtime, visual);
    let status = visual
        .debug_bounds("extensions-status")
        .expect("状态错误必须保留可点击入口");
    let toolbar = visual
        .debug_bounds("extensions-search")
        .expect("工具栏搜索可见");
    assert!(
        status.origin.y + status.size.height <= toolbar.origin.y,
        "恢复入口应位于页头而不是工具栏: status={status:?} toolbar={toolbar:?}"
    );
    visual.simulate_click(status.center(), Modifiers::default());
    pump(&runtime, visual);
    assert!(
        visual.debug_bounds("extensions-status-retry").is_some(),
        "打开页头入口后必须出现刷新状态的恢复按钮"
    );
    // 入口文字和状态原因来自同一份页面状态；重试按钮可达即证明隐藏的不是
    // 唯一恢复路径，原因仍随该状态的错误字符串进入弹层内容。
    visual.update(|_, app| {
        let page = page.read(app);
        assert_eq!(page.state_error.as_deref(), Some("状态不可读"));
    });
}

#[gpui_kit::test]
fn catalog_source_editor_prefills_and_focuses_input(app: &mut TestAppContext) {
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| page.open_source_editor(window, cx));
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        let input = page.read(app).source_input.clone();
        assert!(page.read(app).source_dialog_open);
        assert_eq!(input.read(app).value().to_string(), "");
        assert!(
            input.read(app).focus_handle(app).is_focused(window),
            "打开弹层后焦点必须交给 URL 输入"
        );
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.service
                .set_source_url("https://mirror.example.com/catalog.json")
                .expect("测试来源可切换");
            page.close_source_dialog(cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| page.open_source_editor(window, cx));
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let input = page.read(app).source_input.clone();
        assert_eq!(
            input.read(app).value().to_string(),
            "https://mirror.example.com/catalog.json",
            "自定义来源必须回填实际地址"
        );
    });
}

/// 设置保存成功回执到达后才切换运行态来源，并作废旧目录、刷新新来源。
#[gpui_kit::test]
fn catalog_source_save_success_switches_source_and_refreshes(app: &mut TestAppContext) {
    let catalog = Arc::new(FakeCatalog::default());
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog.clone(),
    );
    let old = "https://old.example.com/catalog.json";
    let new = "https://mirror.example.com/catalog.json";
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.service.set_source_url(old).expect("旧来源可设置");
        });
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| page.open_source_editor(window, cx));
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| {
            page.source_input
                .update(cx, |input, cx| input.set_value(new, window, cx));
            page.request_source_save(cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert_eq!(page.source_pending.as_deref(), Some(new));
        assert!(page.source_saving, "提交后必须进入保存中");
        assert_eq!(page.current_source_url(), old, "回执前不得提前切换来源");
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| page.apply_catalog_source(new, cx));
    });
    pump(&runtime, visual);
    assert_eq!(
        catalog.refresh_calls.load(Ordering::SeqCst),
        1,
        "来源切换后必须刷新新目录"
    );
    visual.update(|_, app| {
        let page = page.read(app);
        assert_eq!(page.current_source_url(), new);
        assert!(!page.source_dialog_open);
        assert!(!page.source_saving);
        assert!(page.source_pending.is_none());
        assert!(page.catalog.is_none(), "旧来源目录必须整体失效");
    });
}

/// 保存失败时保留输入和当前生效来源，只显示原因，不能静默伪成功。
#[gpui_kit::test]
fn catalog_source_save_failure_keeps_source_and_input(app: &mut TestAppContext) {
    let catalog = Arc::new(FakeCatalog::default());
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog.clone(),
    );
    let old = "https://old.example.com/catalog.json";
    let new = "https://mirror.example.com/catalog.json";
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.service.set_source_url(old).expect("旧来源可设置");
        });
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| page.open_source_editor(window, cx));
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| {
            page.source_input
                .update(cx, |input, cx| input.set_value(new, window, cx));
            page.request_source_save(cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.fail_catalog_source_save("网络不可用".to_owned(), cx)
        });
    });
    pump(&runtime, visual);
    assert_eq!(catalog.refresh_calls.load(Ordering::SeqCst), 0);
    visual.update(|_, app| {
        let page = page.read(app);
        assert_eq!(page.current_source_url(), old, "失败不得切换当前来源");
        assert_eq!(page.source_error.as_deref(), Some("网络不可用"));
        assert!(page.source_dialog_open, "失败后弹层保持打开");
        assert!(!page.source_saving);
        assert!(page.source_pending.is_none());
        assert_eq!(
            page.source_input.read(app).value().to_string(),
            new,
            "失败必须保留用户输入"
        );
    });
}

/// 离开分类后保存失败的回执只存在于弹层状态；重开编辑器必须保留原因，
/// 丢弃旧 pending，且不能自动重放或复用失败提交。
#[gpui_kit::test]
fn catalog_source_reopen_keeps_background_failure_without_reusing_pending(
    app: &mut TestAppContext,
) {
    let catalog = Arc::new(FakeCatalog::default());
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog,
    );
    let old = "https://old.example.com/catalog.json";
    let new = "https://mirror.example.com/catalog.json";
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.service.set_source_url(old).expect("旧来源可设置");
        });
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| page.open_source_editor(window, cx));
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| {
            page.source_input
                .update(cx, |input, cx| input.set_value(new, window, cx));
            page.request_source_save(cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            assert!(page.source_saving, "提交后必须进入保存中");
            // 模拟用户离开设置分类：窗口级弹层收起，后台保存继续回读。
            page.close_source_dialog(cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.fail_catalog_source_save("回读失败".to_owned(), cx)
        });
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| page.open_source_editor(window, cx));
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert_eq!(
            page.source_error.as_deref(),
            Some("回读失败"),
            "重开编辑器不能吞掉后台失败原因"
        );
        assert!(page.source_pending.is_none(), "重开不能复用失败提交");
        assert!(!page.source_saving);
        assert_eq!(page.current_source_url(), old, "失败回执不能切换当前来源");
        assert!(page.source_dialog_open);
    });
}

/// 恢复官方默认以空串提交；成功后来源回到 application 的官方常量。
#[gpui_kit::test]
fn catalog_source_restore_default_applies_official_source(app: &mut TestAppContext) {
    let catalog = Arc::new(FakeCatalog::default());
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog.clone(),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.service
                .set_source_url("https://mirror.example.com/catalog.json")
                .expect("自定义来源可设置");
        });
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| page.open_source_editor(window, cx));
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        page.update(app, |page, cx| {
            page.source_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            page.request_source_save(cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        page.update(app, |page, cx| page.apply_catalog_source("", cx));
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert_eq!(
            page.read(app).current_source_url(),
            DEFAULT_PLUGIN_CATALOG_URL,
            "空串必须回到官方来源"
        );
    });
}

/// 写入期间不能打开来源编辑；关闭后延迟打开不能复活弹层；保存中不能重复提交。
#[gpui_kit::test]
fn catalog_source_editor_respects_gates_and_close_cancels_open(app: &mut TestAppContext) {
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, cx| {
            page.writing = true;
            page.open_source_editor(window, cx);
            assert!(!page.source_dialog_open, "写入期间不能打开来源编辑");
            page.writing = false;
            page.open_source_editor(window, cx);
            assert!(page.source_dialog_open);
            // 在延迟打开真正执行前关闭，代次必须让旧回调失效。
            page.close_source_dialog(cx);
            page.source_saving = true;
            page.request_source_save(cx);
            assert!(page.source_pending.is_none(), "保存中不能重复提交");
        });
    });
    pump(&runtime, visual);
    assert!(
        !visual.update(|_, app| page.read(app).source_dialog_open),
        "关闭后延迟打开不能复活弹层"
    );
}

/// 窄窗返回入口必须保持内容宽度并贴左，不能被详情行容器拉满后居中。
#[gpui_kit::test]
fn narrow_back_to_list_stays_content_sized(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.aa").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(id.clone()), cx)
        });
    });
    visual.simulate_resize(size(px(400.), px(600.)));
    pump(&runtime, visual);
    let back = visual
        .debug_bounds("extension-back-to-list")
        .expect("窄窗返回入口可见");
    assert!(
        back.size.width < px(200.),
        "返回入口不应被行容器拉满: {:?}",
        back.size
    );
}

/// 键盘滚动索引直接复用渲染条目：组标题占一个子元素序号，折叠组只留标题。
#[test]
fn list_entries_keep_header_and_row_indices_aligned() {
    // 组归属已由相邻 Header 条目表达，行结构不再携带 group；
    // 参数保留只为让调用处直白标注该行属于哪个分组。
    fn row(_group: RowGroup, id: &str) -> ExtensionRow {
        ExtensionRow {
            element_key: id.to_owned(),
            selection: SelectedEntry::Plugin(PluginId::parse(id).unwrap()),
            name: id.to_owned(),
            description: String::new(),
            progress: None,
            status: None,
            action: None,
            installing: false,
            cancel_enabled: false,
            selected: false,
        }
    }
    let entries = vec![
        ListEntry::Header {
            group: RowGroup::External,
            count: 2,
        },
        ListEntry::Row(Box::new(row(RowGroup::External, "acme.cc"))),
        ListEntry::Row(Box::new(row(RowGroup::External, "acme.dd"))),
        ListEntry::Header {
            group: RowGroup::Builtin,
            count: 1,
        },
        ListEntry::Row(Box::new(row(RowGroup::Builtin, "kerminal.sftp"))),
        ListEntry::Header {
            group: RowGroup::Market,
            count: 1,
        },
        ListEntry::Row(Box::new(row(RowGroup::Market, "acme.aa"))),
    ];
    assert_eq!(row_entry_indices(&entries), vec![1, 2, 4, 6]);
}

/// 已安装行优先使用可信目录元数据，不能把市场插件显示成纯 ID 或“目录不可用”。
#[gpui_kit::test]
fn installed_rows_prefer_catalog_metadata(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let mut plugin = catalog_plugin(id.as_str());
    plugin.name.insert("en".to_owned(), "Notes".to_owned());
    plugin
        .description
        .insert("en".to_owned(), "Search your notes".to_owned());
    let snapshot = catalog_snapshot(vec![plugin]);
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.builtin_expanded = false;
        });
        let rows = page.read(app).visible_rows(&snapshot.catalog.plugins, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Notes");
        assert_eq!(rows[0].description, "Search your notes");

        page.update(app, |page, _| page.catalog = None);
        let rows = page.read(app).visible_rows(&[], "");
        assert!(
            rows[0].description.contains("本地安装或目录中不可用")
                || rows[0].description.contains("unavailable in the catalog"),
            "{}",
            rows[0].description
        );
    });
}

/// 窄窗方向键只移动高亮并保持列表可见；Enter/点击才打开详情。
#[gpui_kit::test]
fn narrow_arrow_selection_keeps_list_visible(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let second = PluginId::parse("acme.bb").unwrap();
    let snapshot = catalog_snapshot(vec![
        catalog_plugin(first.as_str()),
        catalog_plugin(second.as_str()),
    ]);
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.content_width.set(Some(px(100.)));
        });
        page.update(app, |page, cx| {
            page.select_entry(SelectedEntry::Plugin(first.clone()), false, false, cx)
        });
        page.update(app, |page, cx| page.move_selection(1, false, cx));
        let page = page.read(app);
        assert_eq!(page.selected, Some(SelectedEntry::Plugin(second.clone())));
        assert!(page.back_to_list, "窄窗方向键不能立刻隐藏列表");
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(second.clone()), cx)
        });
        assert!(!page.read(app).back_to_list, "显式选择应打开详情");
    });
}

/// 权威状态未知时中央写门禁必须拒绝旧帧到达的写操作，而不只依赖按钮 disabled。
#[gpui_kit::test]
fn write_gate_blocks_operations_when_state_unknown(app: &mut TestAppContext) {
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    let change_calls = store.change_calls.clone();
    let (_, page, visual) = page_fixture(app, store, Arc::new(FakeCatalog::default()));
    let sftp = PluginId::parse("kerminal.sftp").unwrap();
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.state = Some(PluginStoreState::default());
            page.state_error = Some("状态不可读".to_owned());
            page.active = true;
            page.run_operation(Operation::Change(ExtensionChange::Enable(sftp.clone())), cx);
        });
        let page = page.read(app);
        assert!(!page.busy);
        assert!(!page.writing);
        assert!(page.status.is_none(), "门禁拒绝不应伪造错误");
    });
    assert_eq!(change_calls.load(Ordering::SeqCst), 0);

    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.state = None;
            page.state_error = None;
            page.run_operation(Operation::Change(ExtensionChange::Enable(sftp.clone())), cx);
        });
    });
    assert_eq!(change_calls.load(Ordering::SeqCst), 0);
}

/// 已提交但回读失败时所有写入口停用；安全回读成功后才恢复。
#[gpui_kit::test]
fn committed_readback_failure_blocks_writes_until_refresh(app: &mut TestAppContext) {
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    store.fail_state_after_change.store(true, Ordering::SeqCst);
    let change_calls = store.change_calls.clone();
    let fail_state = store.fail_state.clone();
    let (runtime, page, visual) = page_fixture(app, store, Arc::new(FakeCatalog::default()));
    let sftp = PluginId::parse("kerminal.sftp").unwrap();
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.run_operation(Operation::Change(ExtensionChange::Enable(sftp.clone())), cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.state_error.is_some(), "回读失败必须停写");
        assert!(!page.busy);
        assert!(!page.writing);
        assert!(page.retry_operation.is_none(), "已提交事实不能变成重试");
        assert!(
            page.status
                .as_deref()
                .is_some_and(|status| status.contains("已提交") || status.contains("committed")),
            "{:?}",
            page.status
        );
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.run_operation(
                Operation::Change(ExtensionChange::Disable(sftp.clone())),
                cx,
            );
        });
    });
    assert_eq!(
        change_calls.load(Ordering::SeqCst),
        1,
        "状态未知时第二次写入必须被门禁拒绝"
    );

    // 恢复路径：状态仓储恢复后刷新状态，门禁解除。
    fail_state.store(false, Ordering::SeqCst);
    visual.update(|_, app| page.update(app, |page, cx| page.activate(cx)));
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.state_error.is_none());
        assert!(page.state.is_some());
    });
}

/// 重新进入扩展分类不能抹掉尚未处理的清理警告和重试意图；恢复入口要留给用户。
#[gpui_kit::test]
fn activate_preserves_cleanup_warning_and_retry(app: &mut TestAppContext) {
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    let id = PluginId::parse("acme.notes").unwrap();
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.status = Some("插件包残留未清理".to_owned());
            page.status_is_warning = true;
            page.feedback_entry = Some(SelectedEntry::Plugin(id.clone()));
            page.retry_operation = Some(RetryOperation {
                operation: Operation::Change(ExtensionChange::Enable(id.clone())),
                feedback: Some(SelectedEntry::Plugin(id.clone())),
            });
            page.activate(cx);
        });
        let page = page.read(app);
        assert_eq!(page.status.as_deref(), Some("插件包残留未清理"));
        assert!(page.status_is_warning);
        assert_eq!(page.feedback_entry, Some(SelectedEntry::Plugin(id.clone())));
        assert!(page.retry_operation.is_some());
    });
    // 初始读取收尾会触发按需刷新；刷新只读目录，不能顺手清掉尚未处理的重试意图。
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert_eq!(page.status.as_deref(), Some("插件包残留未清理"));
        assert!(page.status_is_warning);
        assert!(page.retry_operation.is_some());
    });
}

/// 未知提交不能盲目重试；安全回读成功后清除停写门禁，但保留提示。
#[gpui_kit::test]
fn outcome_unknown_reads_back_and_clears_retry(app: &mut TestAppContext) {
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    store.change_outcome_unknown.store(true, Ordering::SeqCst);
    let change_calls = store.change_calls.clone();
    let state_reads = store.state_reads.clone();
    let (runtime, page, visual) = page_fixture(app, store, Arc::new(FakeCatalog::default()));
    let sftp = PluginId::parse("kerminal.sftp").unwrap();
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.run_operation(Operation::Change(ExtensionChange::Enable(sftp.clone())), cx);
        });
    });
    pump(&runtime, visual);
    // 未知提交后的安全回读包含状态与 drop-in 两个任务；再推进一轮，确认读盘忙态
    // 已释放，第二次写入才能证明门禁真正恢复。
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy, "回读完成后读盘忙态必须释放");
        assert!(page.retry_operation.is_none(), "未知提交不能保留重试");
        assert!(page.state_error.is_none(), "安全回读成功后应解除停写门禁");
        assert!(page.status.is_some());
        assert!(state_reads.load(Ordering::SeqCst) >= 1);
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.run_operation(
                Operation::Change(ExtensionChange::Disable(sftp.clone())),
                cx,
            );
        });
        // 写入口同步置忙就证明门禁已恢复；真正的存储调用在共享执行器上异步发生，
        // 必须推进运行时后再断言调用次数，否则会把“尚未调度”误判成“被拒绝”。
        assert!(page.read(app).busy, "回读成功后写入口应恢复");
    });
    pump(&runtime, visual);
    assert_eq!(
        change_calls.load(Ordering::SeqCst),
        2,
        "回读成功后写入口应恢复"
    );
}

/// 市场网络刷新只占 refreshing，不占用写忙态；本地已安装管理可在刷新期间继续。
#[gpui_kit::test]
fn catalog_refresh_does_not_hold_write_busy(app: &mut TestAppContext) {
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    let (state_tx, state_rx) = oneshot::channel();
    *store.state_gate.lock().unwrap() = Some(state_rx);
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = empty_catalog_snapshot();
    *catalog.refresh_snapshot.lock().unwrap() = Some(snapshot.clone());
    let (refresh_tx, refresh_rx) = oneshot::channel();
    *catalog.refresh_gate.lock().unwrap() = Some(refresh_rx);
    let refresh_calls = catalog.refresh_calls.clone();
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    let sftp = PluginId::parse("kerminal.sftp").unwrap();
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(PluginStoreState::default());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        page.update(app, |page, cx| page.refresh(cx));
        let page = page.read(app);
        assert!(page.refreshing);
        assert!(!page.busy, "目录刷新不能占用写忙态");
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.run_operation(Operation::Change(ExtensionChange::Enable(sftp.clone())), cx);
        });
        let page = page.read(app);
        assert!(page.busy);
        assert!(page.writing, "刷新期间本地写入应被允许");
    });
    let _ = refresh_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.refreshing);
        assert!(page.busy, "刷新收尾不能释放正在写入的 busy");
        assert!(page.writing);
    });
    let _ = state_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy);
        assert!(!page.writing);
        assert!(page.state.as_ref().is_some_and(|state| {
            state
                .pending
                .iter()
                .any(|change| change.plugin_id() == &sftp)
        }));
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    });
}

/// 目录刷新失败后，同一个固定尺寸刷新按钮立即重试；失败不新增错误条、不清空
/// 旧目录与选择，页头和工具栏位置保持不动，原因进入按钮 tooltip 与无障碍名。
#[gpui_kit::test]
fn catalog_refresh_failure_retries_from_same_button_without_moving_list(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    *catalog.cached_snapshot.lock().unwrap() = Some(snapshot.clone());
    let refresh_calls = catalog.refresh_calls.clone();
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        catalog.clone(),
    );
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(state.clone());
            page.active = true;
            page.select_entry(SelectedEntry::Plugin(id.clone()), false, false, cx);
        });
    });
    // 窄窗下验证固定槽位：错误出现不能把工具栏和下方列表向下推。
    visual.simulate_resize(size(px(400.), px(600.)));
    pump(&runtime, visual);
    let search_before = visual
        .debug_bounds("extensions-search")
        .expect("工具栏搜索可见");
    let refresh_before = visual
        .debug_bounds("extensions-refresh")
        .expect("刷新按钮可见");
    visual.simulate_click(refresh_before.center(), Modifiers::default());
    pump(&runtime, visual);
    assert_eq!(
        refresh_calls.load(Ordering::SeqCst),
        1,
        "第一次点击必须真实请求目录"
    );
    let error = visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.refreshing, "失败后必须结束刷新忙态");
        assert!(!page.busy, "只读刷新失败不能留下页面忙态");
        assert!(page.catalog.is_some(), "失败不能清空旧目录");
        assert_eq!(
            page.selected,
            Some(SelectedEntry::Plugin(id.clone())),
            "失败不能丢掉当前选择"
        );
        page.catalog_error.clone().expect("失败必须记录原因")
    });
    let search_after = visual
        .debug_bounds("extensions-search")
        .expect("失败后工具栏搜索仍可见");
    let refresh_after = visual
        .debug_bounds("extensions-refresh")
        .expect("失败后刷新按钮仍可见");
    assert_eq!(
        (search_after.origin, search_after.size),
        (search_before.origin, search_before.size),
        "失败不能挤动工具栏或下方列表"
    );
    assert_eq!(
        (refresh_after.origin, refresh_after.size),
        (refresh_before.origin, refresh_before.size),
        "刷新按钮必须保持固定尺寸"
    );
    // 错误只进入同一按钮的 tooltip/无障碍名，不新增独立感叹号或错误横幅。
    let expected = crate::locale::text(
        format!("刷新目录失败：{error}。点击重试"),
        format!("Catalog refresh failed: {error}. Click to retry."),
    );
    visual.update(|window, _| {
        let found = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .any(|node| {
                node.visible()
                    && node.role() == Some(Role::Button)
                    && node.label().is_some_and(|label| label == expected.as_ref())
            });
        assert!(found, "刷新按钮的无障碍名必须包含失败原因与重试动作");
    });
    // 让下一次刷新成功，再点同一个按钮坐标证明立即重试走的是同一入口。
    *catalog.refresh_snapshot.lock().unwrap() = Some(snapshot.clone());
    visual.simulate_click(refresh_after.center(), Modifiers::default());
    pump(&runtime, visual);
    assert_eq!(
        refresh_calls.load(Ordering::SeqCst),
        2,
        "同一刷新按钮必须立即发起第二次请求"
    );
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.catalog_error.is_none(), "重试成功后必须清除目录错误");
        assert!(!page.refreshing);
        assert!(!page.busy);
        assert!(page.catalog.is_some());
        assert_eq!(
            page.selected,
            Some(SelectedEntry::Plugin(id.clone())),
            "重试成功仍应保留选择"
        );
    });
}

/// stale 回退是网络失败后的旧缓存，不是一次成功刷新：保留正在查看的目录、选择
/// 和非目录重试，只让固定刷新按钮原地报错；不新增副标题或第二个重试入口。
#[gpui_kit::test]
fn catalog_stale_fallback_keeps_state_and_retries_from_same_button(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let catalog = Arc::new(FakeCatalog::default());
    let fresh = {
        let mut snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
        snapshot.fetched_at = 1_700_000_000;
        snapshot
    };
    let mut stale = fresh.clone();
    stale.stale = true;
    // 回退缓存与界面正在看的目录不是同一份；只有真正被替换时 fetched_at 才会变化。
    stale.fetched_at = 42;
    *catalog.cached_snapshot.lock().unwrap() = Some(fresh.clone());
    *catalog.refresh_snapshot.lock().unwrap() = Some(stale);
    let refresh_calls = catalog.refresh_calls.clone();
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        catalog.clone(),
    );
    let retry = RetryOperation {
        operation: Operation::Change(ExtensionChange::Enable(id.clone())),
        feedback: Some(SelectedEntry::Plugin(id.clone())),
    };
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.catalog = Some(fresh.clone());
            page.state = Some(state.clone());
            page.active = true;
            page.select_entry(SelectedEntry::Plugin(id.clone()), false, false, cx);
            page.retry_operation = Some(retry);
        });
    });
    // 与真实失败路径一样在窄窗验证固定槽位：stale 不新增副标题或错误条。
    visual.simulate_resize(size(px(400.), px(600.)));
    pump(&runtime, visual);
    let search_before = visual
        .debug_bounds("extensions-search")
        .expect("工具栏搜索可见");
    let refresh_before = visual
        .debug_bounds("extensions-refresh")
        .expect("刷新按钮可见");
    visual.simulate_click(refresh_before.center(), Modifiers::default());
    pump(&runtime, visual);
    assert_eq!(
        refresh_calls.load(Ordering::SeqCst),
        1,
        "第一次点击必须真实请求目录"
    );
    let (error, header) = visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.refreshing, "stale 回退后必须结束刷新忙态");
        assert!(!page.busy, "只读刷新不能留下页面忙态");
        assert!(page.offline, "stale 回退必须阻止基于过期目录更新");
        assert_eq!(
            page.catalog.as_ref().map(|snapshot| snapshot.fetched_at),
            Some(1_700_000_000),
            "stale 回退不能替换正在查看的目录"
        );
        assert_eq!(
            page.selected,
            Some(SelectedEntry::Plugin(id.clone())),
            "stale 回退不能丢掉当前选择"
        );
        assert!(
            matches!(
                &page.retry_operation,
                Some(RetryOperation {
                    operation: Operation::Change(ExtensionChange::Enable(retry_id)),
                    ..
                }) if retry_id == &id
            ),
            "目录失败不能清掉非目录操作的重试"
        );
        (
            page.catalog_error.clone().expect("stale 回退必须记录原因"),
            page.header_status(),
        )
    });
    assert!(header.is_none(), "stale 回退不能新增“离线缓存”等页头副标题");
    let search_after = visual
        .debug_bounds("extensions-search")
        .expect("stale 回退后工具栏搜索仍可见");
    let refresh_after = visual
        .debug_bounds("extensions-refresh")
        .expect("stale 回退后刷新按钮仍可见");
    assert_eq!(
        (search_after.origin, search_after.size),
        (search_before.origin, search_before.size),
        "stale 回退不能挤动工具栏或下方列表"
    );
    assert_eq!(
        (refresh_after.origin, refresh_after.size),
        (refresh_before.origin, refresh_before.size),
        "刷新按钮必须保持固定尺寸与位置"
    );
    let expected = crate::locale::text(
        format!("刷新目录失败：{error}。点击重试"),
        format!("Catalog refresh failed: {error}. Click to retry."),
    );
    visual.update(|window, _| {
        let found = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .any(|node| {
                node.visible()
                    && node.role() == Some(Role::Button)
                    && node.label().is_some_and(|label| label == expected.as_ref())
            });
        assert!(found, "刷新按钮必须承载 stale 原因与重试动作");
    });
    // 同一个按钮位置再次点击即可走真实网络重试；成功后恢复在线并清掉旧原因。
    *catalog.refresh_snapshot.lock().unwrap() = Some(fresh.clone());
    visual.simulate_click(refresh_after.center(), Modifiers::default());
    pump(&runtime, visual);
    assert_eq!(
        refresh_calls.load(Ordering::SeqCst),
        2,
        "同一刷新按钮必须立即发起第二次请求"
    );
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.offline, "成功刷新后必须恢复在线");
        assert!(page.catalog_error.is_none(), "成功刷新后必须清除目录错误");
        assert!(
            page.retry_operation.is_none(),
            "显式刷新成功后旧的非目录重试入口应清掉"
        );
        assert_eq!(
            page.selected,
            Some(SelectedEntry::Plugin(id.clone())),
            "重试成功仍应保留选择"
        );
        assert_eq!(
            page.catalog.as_ref().map(|snapshot| snapshot.fetched_at),
            Some(fresh.fetched_at)
        );
    });
}

/// 同源 stale 回退在页面还没有目录时是唯一可用的内容，必须直接补上，不能
/// 提示正在展示一份并不存在的旧列表；已有选择和非目录重试仍保持不动。
#[gpui_kit::test]
fn catalog_stale_fallback_adopts_cache_when_page_has_no_catalog(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let catalog = Arc::new(FakeCatalog::default());
    let mut stale = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    stale.stale = true;
    stale.fetched_at = 42;
    *catalog.refresh_snapshot.lock().unwrap() = Some(stale.clone());
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        catalog,
    );
    let retry = RetryOperation {
        operation: Operation::Change(ExtensionChange::Enable(id.clone())),
        feedback: Some(SelectedEntry::Plugin(id.clone())),
    };
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.catalog = None;
            page.state = Some(state.clone());
            page.active = true;
            page.select_entry(SelectedEntry::Plugin(id.clone()), false, false, cx);
            page.retry_operation = Some(retry);
        });
    });
    pump(&runtime, visual);
    let refresh = visual
        .debug_bounds("extensions-refresh")
        .expect("刷新按钮可见");
    visual.simulate_click(refresh.center(), Modifiers::default());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.offline, "stale 回退必须标记离线");
        assert_eq!(
            page.catalog.as_ref().map(|snapshot| snapshot.fetched_at),
            Some(42),
            "页面没有目录时同源 stale 缓存必须补上"
        );
        assert_eq!(
            page.selected,
            Some(SelectedEntry::Plugin(id.clone())),
            "补缓存不能清掉已安装选择"
        );
        assert!(page.retry_operation.is_some(), "目录失败不能清掉非目录重试");
        assert!(
            page.catalog_error
                .as_deref()
                .is_some_and(|message| message.contains("缓存") || message.contains("cached")),
            "提示必须说明展示的是缓存而不是旧目录"
        );
        assert!(
            page.header_status().is_none(),
            "stale 回退不能新增页头副标题"
        );
    });
}

/// 目录失败且整页没有任何其他组可渲染时只保留空态说明；重试仍只走页头
/// 固定尺寸的刷新键，空态不再生成第二个目录重试入口。
#[gpui_kit::test]
fn catalog_empty_state_has_no_second_retry_entry(app: &mut TestAppContext) {
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.builtin_units.clear();
            page.catalog = None;
            page.catalog_error = Some("目录不可用".to_owned());
            page.state = Some(PluginStoreState::default());
            page.state_error = None;
            page.active = true;
        });
    });
    pump(&runtime, visual);
    assert!(
        visual.debug_bounds("extensions-refresh").is_some(),
        "页头刷新键必须是唯一目录重试入口"
    );
    assert!(
        visual.debug_bounds("extensions-market-retry").is_none(),
        "空态不能出现第二个目录重试入口"
    );
}

/// 目录错误已有固定刷新按钮，不能重复混进状态弹层的原因列表。
#[gpui_kit::test]
fn catalog_error_is_not_repeated_in_status_popover(app: &mut TestAppContext) {
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog_error = Some("目录刷新失败".to_owned());
            page.status = Some("安装失败".to_owned());
            page.status_is_warning = false;
        });
        let messages = page.read(app).status_messages();
        assert!(
            messages
                .iter()
                .all(|(_, message)| message != "目录刷新失败"),
            "目录错误不能混进状态弹层"
        );
        assert!(
            messages.iter().any(|(_, message)| message == "安装失败"),
            "操作失败仍必须保留在状态弹层"
        );
    });
}

/// 初始缓存读取没有取消参数，界面不能显示实际无效的取消按钮；网络刷新阶段才可取消。
#[gpui_kit::test]
fn initial_catalog_load_does_not_offer_fake_cancel(app: &mut TestAppContext) {
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = empty_catalog_snapshot();
    *catalog.cached_snapshot.lock().unwrap() = Some(snapshot.clone());
    *catalog.refresh_snapshot.lock().unwrap() = Some(snapshot.clone());
    let (cached_tx, cached_rx) = oneshot::channel();
    *catalog.cached_gate.lock().unwrap() = Some(cached_rx);
    let (refresh_tx, refresh_rx) = oneshot::channel();
    *catalog.refresh_gate.lock().unwrap() = Some(refresh_rx);
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, cx| page.activate(cx));
        let page = page.read(app);
        assert!(page.initial_catalog_loading);
        assert!(
            !page.can_cancel_catalog_refresh(),
            "初始缓存读取不能提供假取消"
        );
    });
    let _ = cached_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.initial_catalog_loading);
        assert!(page.can_cancel_catalog_refresh(), "网络刷新阶段应提供取消");
    });
    let _ = refresh_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.can_cancel_catalog_refresh());
        assert!(!page.refreshing);
    });
}

/// 离开分类后旧预检结果即使返回 Ok 也不能打开确认或释放新一轮读取的 busy。
#[gpui_kit::test]
fn late_preflight_after_reactivate_is_discarded(app: &mut TestAppContext) {
    let store = Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new()));
    let (inspect_tx, inspect_rx) = oneshot::channel();
    *store.inspect_gate.lock().unwrap() = Some(inspect_rx);
    let inspect_calls = store.inspect_calls.clone();
    let (state_tx, state_rx) = oneshot::channel();
    *store.state_gate.lock().unwrap() = Some(state_rx);
    let catalog = Arc::new(FakeCatalog::default());
    let snapshot = empty_catalog_snapshot();
    *catalog.cached_snapshot.lock().unwrap() = Some(snapshot.clone());
    let (cached_tx, cached_rx) = oneshot::channel();
    *catalog.cached_gate.lock().unwrap() = Some(cached_rx);
    let (runtime, page, visual) = page_fixture(app, store, catalog);
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(PluginStoreState::default());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.inspect_file(PathBuf::from("local.kplugin"), cx)
        });
        let page = page.read(app);
        assert!(page.busy);
        assert!(page.inspecting);
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| page.deactivate(cx));
        let page = page.read(app);
        assert!(!page.busy);
        assert!(!page.inspecting);
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| page.activate(cx));
        assert!(page.read(app).busy, "重新进入应启动新一轮读取");
    });
    let _ = inspect_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.confirmation.is_none(), "旧预检不能弹出确认");
        assert!(!page.inspecting);
        assert!(page.busy, "旧回执不能释放新一轮读取的 busy");
    });
    let _ = cached_tx.send(());
    let _ = state_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy);
        assert!(page.confirmation.is_none());
        assert_eq!(inspect_calls.load(Ordering::SeqCst), 1);
    });
}

/// 在途写入的 busy 不能被初始读取收尾或离开分类覆盖，否则 writing 会永久挂住。
#[gpui_kit::test]
fn writing_busy_survives_initial_load_and_deactivate(app: &mut TestAppContext) {
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.writing = true;
            page.busy = true;
            page.generation = 7;
            page.catalog_error = Some("测试不刷新".to_owned());
            page.deactivate(cx);
        });
        let page = page.read(app);
        assert!(page.writing);
        assert!(page.busy);
        assert_eq!(page.generation, 7, "在途写入代次不能被离开分类作废");
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            let pending = std::cell::Cell::new(1);
            finish_initial_load(page, &pending, cx);
        });
        let page = page.read(app);
        assert!(page.busy, "初始读取收尾不能释放写入 busy");
        assert!(page.writing);
    });
    visual.update(|_, app| {
        page.update(app, |page, cx| {
            page.writing = false;
            page.busy = true;
            let pending = std::cell::Cell::new(1);
            finish_initial_load(page, &pending, cx);
        });
        assert!(!page.read(app).busy, "无写入时初始读取收尾应释放 busy");
    });
}

/// 窄窗 Enter 打开详情后焦点必须落到已挂载详情；Escape 返回列表并恢复列表焦点，
/// 窗口级确认打开时详情不能抢走 Escape。
#[gpui_kit::test]
fn narrow_detail_focus_handles_escape_without_closing_settings(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let second = PluginId::parse("acme.bb").unwrap();
    let snapshot = catalog_snapshot(vec![
        catalog_plugin(first.as_str()),
        catalog_plugin(second.as_str()),
    ]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select_entry(SelectedEntry::Plugin(first.clone()), false, false, cx)
        });
        let list_focus = page.read(app).list_focus.clone();
        window.focus(&list_focus, app);
    });
    // 分栏阈值来自真实测量宽度；先把窗口缩到单栏区间，再让绘制写入 content_width，
    // 否则测试窗口默认 900px 会走宽窗分栏，Enter 不会把焦点交给详情。
    visual.simulate_resize(size(px(400.), px(600.)));
    pump(&runtime, visual);
    visual.simulate_keystrokes("down enter");
    pump(&runtime, visual);
    visual.update(|window, app| {
        let back_to_list = page.read(app).back_to_list;
        let detail_focused = page.read(app).detail_focus.is_focused(window);
        let list_focused = page.read(app).list_focus.is_focused(window);
        assert!(!back_to_list);
        assert!(detail_focused, "Enter 打开详情后焦点应落到实际挂载的详情");
        assert!(!list_focused, "不能保留已卸载列表的焦点");
        // 确认弹层打开时焦点属于 Kit 顶层，详情不能吞掉它的 Escape。
        page.update(app, |page, _| {
            page.confirmation = Some(Confirmation::Install(
                selection(&catalog_plugin("acme.bb"), "1.0.0", Some(&test_identity())).unwrap(),
            ));
            page.dialog_open = true;
        });
    });
    visual.simulate_keystrokes("escape");
    visual.update(|_, app| {
        assert!(
            !page.read(app).back_to_list,
            "确认弹层的 Escape 不能被详情吞掉"
        );
        page.update(app, |page, _| {
            page.confirmation = None;
            page.dialog_open = false;
        });
    });
    visual.simulate_keystrokes("escape");
    visual.update(|window, app| {
        let state = page.read(app);
        assert!(state.back_to_list);
        assert!(
            state.list_focus.is_focused(window),
            "Escape 返回列表并恢复列表焦点"
        );
    });
}

/// 窄窗方向键高亮不读取不可见详情；Enter 打开详情才请求，同项重复 Enter 不重发。
#[gpui_kit::test]
fn readme_loads_only_when_detail_visible_and_dedupes(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let second = PluginId::parse("acme.bb").unwrap();
    let snapshot = catalog_snapshot(vec![
        catalog_plugin(first.as_str()),
        catalog_plugin(second.as_str()),
    ]);
    let catalog = Arc::new(FakeCatalog::default());
    let calls = catalog.readme_calls.clone();
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog,
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
            page.content_width.set(Some(px(100.)));
        });
        page.update(app, |page, cx| {
            page.select_entry(SelectedEntry::Plugin(first.clone()), false, false, cx)
        });
        page.update(app, |page, cx| page.move_selection(1, false, cx));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "详情不可见时不应请求 README"
        );
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(second.clone()), cx)
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(second.clone()), cx)
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "同一项重复 Enter 不应重复请求"
        );
        assert_eq!(page.read(app).readme.as_deref(), Some("readme:acme.bb"));
    });
}

/// 切换选择时旧 README 请求被取消，迟到回执不能覆盖新选择的正文。
#[gpui_kit::test]
fn readme_switch_discards_previous_request(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let second = PluginId::parse("acme.bb").unwrap();
    let snapshot = catalog_snapshot(vec![
        catalog_plugin(first.as_str()),
        catalog_plugin(second.as_str()),
    ]);
    let catalog = Arc::new(FakeCatalog::default());
    let (gate_tx, gate_rx) = oneshot::channel();
    *catalog.readme_gate.lock().unwrap() = Some(gate_rx);
    let calls = catalog.readme_calls.clone();
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog,
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(first.clone()), cx)
        });
    });
    // 让第一次请求真正进入闸门，再切换选择，才能观察到取消与迟到回执。
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(second.clone()), cx)
        });
    });
    pump(&runtime, visual);
    visual.update(|_, _| {
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    });
    let _ = gate_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert_eq!(
            page.read(app).readme.as_deref(),
            Some("readme:acme.bb"),
            "旧选择迟到的 README 不能覆盖新选择"
        );
    });
}

/// 行内更新按钮与批量更新共用同一可更新判定；状态未知时两者一起失效。
#[gpui_kit::test]
fn update_action_and_batch_share_eligibility(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let mut plugin = catalog_plugin(id.as_str());
    plugin.versions[0].version = PluginVersion::parse("2.0.0").unwrap();
    let snapshot = catalog_snapshot(vec![plugin]);
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
        });
        let rows = page.read(app).visible_rows(&snapshot.catalog.plugins, "");
        assert_eq!(rows.len(), 1, "已安装行仍应可见");
        let action = rows[0].action.as_ref().expect("可更新行必须保留更新按钮");
        assert!(!action.disabled);
        assert_eq!(page.read(app).update_candidates().len(), 1);
        assert!(page.read(app).update_selection(&id).is_some());

        page.update(app, |page, _| {
            page.state_error = Some("状态不可读".to_owned());
        });
        let rows = page.read(app).visible_rows(&snapshot.catalog.plugins, "");
        assert!(
            rows[0].action.is_none(),
            "状态未知时不能留下可点击的更新入口"
        );
        assert!(page.read(app).update_candidates().is_empty());
        assert!(page.read(app).update_selection(&id).is_none());
    });
}

/// 批量范围只含真实可更新的已安装记录：pending、未确认投放、不支持与已是最新
/// 都不进入；搜索、分类与折叠不改变范围，状态未知时整体禁用。
#[gpui_kit::test]
fn batch_scope_excludes_pending_unsupported_and_unknown_state(app: &mut TestAppContext) {
    let updatable = PluginId::parse("acme.updatable").unwrap();
    let up_to_date = PluginId::parse("acme.current").unwrap();
    let pending = PluginId::parse("acme.pending").unwrap();
    let unsupported = PluginId::parse("acme.unsupported").unwrap();
    let dropin = PluginId::parse("acme.dropin").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        updatable.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let mut current = installed_record(PluginSource::Official, PluginInstallStatus::Ready);
    current.active = PluginVersion::parse("2.0.0").unwrap();
    state.plugins.insert(up_to_date.clone(), current);
    state.plugins.insert(
        unsupported.clone(),
        installed_record(
            PluginSource::Local,
            PluginInstallStatus::Unsupported {
                reason: PluginUnsupportedReason::ReservedIdentity,
            },
        ),
    );
    state.plugins.insert(
        dropin.clone(),
        installed_record(PluginSource::DropIn, PluginInstallStatus::Ready),
    );
    state.pending.push(PendingPluginChange::Install {
        id: pending.clone(),
        to: PluginVersion::parse("2.0.0").unwrap(),
        staged: "staged".into(),
        fingerprint: None,
        source: PluginSource::Local,
        installed_at: "2026-01-01".into(),
    });
    let plugins: Vec<CatalogPlugin> = [&updatable, &up_to_date, &pending, &unsupported, &dropin]
        .into_iter()
        .map(|id| {
            let mut plugin = catalog_plugin(id.as_str());
            plugin.versions[0].version = PluginVersion::parse("2.0.0").unwrap();
            plugin
        })
        .collect();
    let snapshot = catalog_snapshot(plugins);
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        let candidates = page.read(app).update_candidates();
        assert_eq!(
            candidates
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            vec![updatable.clone()],
            "只有已安装且目录版本更高的记录进入批量范围"
        );

        // 搜索与分类只影响展示，不缩小或扩大批量范围。
        page.update(app, |page, cx| {
            page.filter.category = Some("不存在".to_owned());
            page.search.update(cx, |input, cx| {
                input.set_value("zzz".to_owned(), window, cx)
            });
        });
        assert_eq!(page.read(app).update_candidates().len(), 1);
        assert!(!page.read(app).builtin_expanded);

        page.update(app, |page, cx| page.open_batch_update(cx));
        let confirmation = page.read(app).confirmation.clone();
        let Some(Confirmation::BatchUpdate(plan)) = confirmation else {
            panic!("批量更新必须打开一次确认");
        };
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].id, updatable);
        assert_eq!(plan.items[0].from, "1.0.0");
        assert_eq!(plan.items[0].selection.version.to_string(), "2.0.0");
        assert!(!plan.items[0].full_access);

        page.update(app, |page, _| {
            page.confirmation = None;
            page.state_error = Some("状态不可读".to_owned());
        });
        page.update(app, |page, cx| page.open_batch_update(cx));
        assert!(
            page.read(app).confirmation.is_none(),
            "状态未知时不能打开批量确认"
        );
        assert!(page.read(app).update_candidates().is_empty());
    });
}

/// 批量失败停在安全边界：保留已提交计数、未处理数量与重试入口，并回读权威状态。
#[gpui_kit::test]
fn batch_failure_reports_partial_and_allows_replan(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let mut plugin = catalog_plugin(id.as_str());
    plugin.versions[0].version = PluginVersion::parse("2.0.0").unwrap();
    let snapshot = catalog_snapshot(vec![plugin]);
    let store = Arc::new(FakeStore::new(state.clone(), Vec::new()));
    let state_reads = store.state_reads.clone();
    let (runtime, page, visual) = page_fixture(app, store, Arc::new(FakeCatalog::default()));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        page.update(app, |page, cx| page.open_batch_update(cx));
        let plan = match page.read(app).confirmation.clone() {
            Some(Confirmation::BatchUpdate(plan)) => plan,
            _ => panic!("批量确认应已打开"),
        };
        page.update(app, |page, cx| {
            page.finish_confirmation(Some(Operation::BatchUpdate(plan)), cx)
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy, "失败后必须释放写忙态");
        assert!(page.batch.is_none(), "失败后不能残留批量进度");
        assert!(page.retry_batch, "失败后必须提供重新规划入口");
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text("批量更新已停止", "The batch update stopped").to_string()
            )),
            "反馈必须说明批量在失败处停止: {:?}",
            page.status
        );
        assert!(state_reads.load(Ordering::SeqCst) >= 1, "必须回读权威状态");
    });
}

/// 两项已安装记录与更高目录版本：批量测试只关心串行顺序与终态，不复制目录契约。
fn batch_state_and_snapshot(ids: &[&str]) -> (PluginStoreState, CatalogSnapshot) {
    let mut state = PluginStoreState::default();
    let mut plugins = Vec::new();
    for id in ids {
        let id = PluginId::parse(*id).unwrap();
        state.plugins.insert(
            id.clone(),
            installed_record(PluginSource::Official, PluginInstallStatus::Ready),
        );
        let mut plugin = catalog_plugin(id.as_str());
        plugin.versions[0].version = PluginVersion::parse("2.0.0").unwrap();
        plugins.push(plugin);
    }
    (state, catalog_snapshot(plugins))
}

/// 通过真实确认入口启动批量，保证测试覆盖冻结计划与串行执行，而不是直接调用内部循环。
fn start_batch(page: &Entity<ExtensionsPage>, app: &mut gpui_kit::App) {
    page.update(app, |page, cx| page.open_batch_update(cx));
    let plan = match page.read(app).confirmation.clone() {
        Some(Confirmation::BatchUpdate(plan)) => plan,
        _ => panic!("批量确认应已打开"),
    };
    page.update(app, |page, cx| {
        page.finish_confirmation(Some(Operation::BatchUpdate(plan)), cx)
    });
}

/// 成功一项后失败：后续项不再启动，已提交项保留 pending，反馈准确区分已更新与失败。
#[gpui_kit::test]
fn batch_stops_after_failure_and_keeps_committed_pending(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let second = PluginId::parse("acme.bb").unwrap();
    let (state, snapshot) = batch_state_and_snapshot(&["acme.aa", "acme.bb"]);
    let store = Arc::new(FakeStore::new(state.clone(), Vec::new()));
    store
        .install_outcomes
        .lock()
        .unwrap()
        .extend([FakeInstallOutcome::Success, FakeInstallOutcome::Failure]);
    let install_calls = store.install_calls.clone();
    let (runtime, page, visual) =
        page_fixture(app, store.clone(), Arc::new(FakeCatalog::default()));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        start_batch(&page, app);
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy, "失败后必须释放写忙态");
        assert!(page.batch.is_none(), "失败后不能残留批量进度");
        assert!(page.retry_batch, "失败后必须允许重新规划剩余项");
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text("批量更新已停止", "The batch update stopped").to_string()
            )),
            "反馈必须说明批量在失败处停止: {:?}",
            page.status
        );
        assert!(
            page.status.as_deref().is_some_and(|status| status
                .contains(&crate::locale::text("已更新 1 个", "1 updated").to_string())),
            "反馈必须准确报告部分完成: {:?}",
            page.status
        );
        assert_eq!(
            install_calls.lock().unwrap().as_slice(),
            vec![first.clone(), second],
            "失败项之后的扩展不能启动安装"
        );
        let pending = store.state.lock().unwrap().pending.clone();
        assert_eq!(pending.len(), 1, "只有已提交项保留 pending");
        assert!(matches!(
            &pending[0],
            PendingPluginChange::Update { id, .. } if id == &first
        ));
    });
}

/// 成功一项后结果未知：停在安全边界并回读，不能凭错误文案继续或重放。
#[gpui_kit::test]
fn batch_stops_after_unknown_and_requires_readback(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let (state, snapshot) = batch_state_and_snapshot(&["acme.aa", "acme.bb"]);
    let store = Arc::new(FakeStore::new(state.clone(), Vec::new()));
    store
        .install_outcomes
        .lock()
        .unwrap()
        .extend([FakeInstallOutcome::Success, FakeInstallOutcome::Unknown]);
    let install_calls = store.install_calls.clone();
    let state_reads = store.state_reads.clone();
    let (runtime, page, visual) =
        page_fixture(app, store.clone(), Arc::new(FakeCatalog::default()));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        start_batch(&page, app);
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy);
        assert!(page.retry_batch, "回读成功后应允许重新规划");
        assert!(
            page.status.as_deref().is_some_and(|status| status
                .contains(&crate::locale::text("结果未知", "is unknown").to_string())),
            "反馈必须说明结果未知: {:?}",
            page.status
        );
        assert_eq!(install_calls.lock().unwrap().len(), 2);
        assert!(
            state_reads.load(Ordering::SeqCst) >= 2,
            "未知结果必须回读权威状态"
        );
        let pending = store.state.lock().unwrap().pending.clone();
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            &pending[0],
            PendingPluginChange::Update { id, .. } if id == &first
        ));
    });
}

/// 两项之间取消：已提交项保留 pending，未开始项不提交，并保留重新规划入口。
#[gpui_kit::test]
fn batch_cancel_between_items_keeps_committed_pending(app: &mut TestAppContext) {
    let (state, snapshot) = batch_state_and_snapshot(&["acme.aa", "acme.bb"]);
    let store = Arc::new(FakeStore::new(state.clone(), Vec::new()));
    store
        .install_outcomes
        .lock()
        .unwrap()
        .extend([FakeInstallOutcome::Success, FakeInstallOutcome::Success]);
    let install_calls = store.install_calls.clone();
    let state_reads = store.state_reads.clone();
    // 冻结第一项提交后的权威回读，让测试有机会在两项之间取消。
    let (gate_tx, gate_rx) = oneshot::channel();
    *store.state_gate.lock().unwrap() = Some(gate_rx);
    let (runtime, page, visual) =
        page_fixture(app, store.clone(), Arc::new(FakeCatalog::default()));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        start_batch(&page, app);
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert_eq!(
            state_reads.load(Ordering::SeqCst),
            1,
            "第一项提交后应开始权威回读"
        );
        assert_eq!(install_calls.lock().unwrap().len(), 1);
        page.update(app, |page, _| page.cancel());
    });
    let _ = gate_tx.send(());
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy);
        assert!(page.batch.is_none());
        assert!(page.retry_batch, "仍有未处理项时应允许重新规划");
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text("已取消批量更新", "Batch update cancelled").to_string()
            )),
            "反馈必须说明取消与已提交数量: {:?}",
            page.status
        );
        assert_eq!(
            install_calls.lock().unwrap().len(),
            1,
            "取消后不能启动第二项"
        );
        assert_eq!(
            store.state.lock().unwrap().pending.len(),
            1,
            "已提交项必须保留 pending，未开始项不能提交"
        );
    });
}

/// 当前项安装被取消：没有已提交项，剩余项不启动，仍允许重新规划。
#[gpui_kit::test]
fn batch_cancel_during_item_reports_uncommitted(app: &mut TestAppContext) {
    let (state, snapshot) = batch_state_and_snapshot(&["acme.aa", "acme.bb"]);
    let store = Arc::new(FakeStore::new(state.clone(), Vec::new()));
    store
        .install_outcomes
        .lock()
        .unwrap()
        .extend([FakeInstallOutcome::Cancelled, FakeInstallOutcome::Success]);
    let install_calls = store.install_calls.clone();
    let (runtime, page, visual) =
        page_fixture(app, store.clone(), Arc::new(FakeCatalog::default()));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        start_batch(&page, app);
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy);
        assert!(page.retry_batch, "取消后仍允许重新规划未处理项");
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text("已取消批量更新", "Batch update cancelled").to_string()
            )),
            "反馈必须说明取消: {:?}",
            page.status
        );
        assert_eq!(
            install_calls.lock().unwrap().len(),
            1,
            "当前项取消后不能启动后续项"
        );
        assert!(
            store.state.lock().unwrap().pending.is_empty(),
            "取消项不能留下 pending"
        );
    });
}

/// 提交成功但权威回读失败：停止写入、报告已提交事实，并禁止直接重试。
#[gpui_kit::test]
fn batch_committed_but_readback_failure_blocks_retry(app: &mut TestAppContext) {
    let (state, snapshot) = batch_state_and_snapshot(&["acme.aa"]);
    let store = Arc::new(FakeStore::new(state.clone(), Vec::new()));
    store
        .install_outcomes
        .lock()
        .unwrap()
        .push_back(FakeInstallOutcome::Success);
    store.fail_state_after_change.store(true, Ordering::SeqCst);
    let (runtime, page, visual) =
        page_fixture(app, store.clone(), Arc::new(FakeCatalog::default()));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        start_batch(&page, app);
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.busy);
        assert!(!page.retry_batch, "状态未知时不能直接重试");
        assert!(page.state_error.is_some(), "回读失败必须进入可恢复错误态");
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text(
                    "已提交，但读取状态失败",
                    "was committed, but its state could not be read",
                )
                .to_string()
            )),
            "反馈必须说明已提交但回读失败: {:?}",
            page.status
        );
        assert_eq!(
            store.state.lock().unwrap().pending.len(),
            1,
            "已提交项仍保留 pending"
        );
    });
}

/// 清理残留是磁盘事实：即使批量成功也必须出现在终态反馈里。
#[gpui_kit::test]
fn batch_surfaces_cleanup_warnings(app: &mut TestAppContext) {
    let (state, snapshot) = batch_state_and_snapshot(&["acme.aa"]);
    let store = Arc::new(FakeStore::new(state.clone(), Vec::new()));
    store
        .install_outcomes
        .lock()
        .unwrap()
        .push_back(FakeInstallOutcome::SuccessWithCleanupWarning);
    let (runtime, page, visual) = page_fixture(app, store, Arc::new(FakeCatalog::default()));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        start_batch(&page, app);
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(!page.retry_batch);
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text("已更新 1 个扩展", "Updated 1 extensions").to_string()
            )),
            "成功路径仍应报告更新数量: {:?}",
            page.status
        );
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text("残留未清理", "left unremoved staging material").to_string()
            )),
            "清理告警不能被成功文案吞掉: {:?}",
            page.status
        );
    });
}

/// 空批量提示属于整页事实：不能挂到旧对象详情，也不能吞掉仍可用的单项恢复入口。
#[gpui_kit::test]
fn empty_batch_feedback_clears_stale_owner_but_keeps_retry(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.aa").unwrap();
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(PluginStoreState::default());
            page.catalog = Some(empty_catalog_snapshot());
            page.active = true;
            // 模拟上一项失败留下的归属与恢复入口；空批量既没有候选也没有
            // 新的失败对象，必须只清归属。
            page.feedback_entry = Some(SelectedEntry::Plugin(id.clone()));
            page.retry_operation = Some(RetryOperation {
                operation: Operation::Delete(id.clone()),
                feedback: Some(SelectedEntry::Plugin(id.clone())),
            });
        });
        page.update(app, |page, cx| page.open_batch_update(cx));
    });
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(
            page.status.as_deref().is_some_and(|status| status.contains(
                &crate::locale::text("没有可更新的扩展", "No updates available").to_string()
            )),
            "空批量必须给出明确反馈: {:?}",
            page.status
        );
        assert!(page.feedback_entry.is_none(), "整页提示不能挂到旧对象详情");
        assert!(
            page.retry_operation.is_some(),
            "空批量不能吞掉仍可用的单项恢复入口"
        );
        assert!(!page.retry_batch, "没有候选时批量重试入口必须消失");
    });
}

/// 更新资格只给市场来源：本地包与已确认投放即使目录版本更高也不能走目录安装。
#[gpui_kit::test]
fn update_eligibility_requires_market_source(app: &mut TestAppContext) {
    let official = PluginId::parse("acme.official").unwrap();
    let local = PluginId::parse("acme.local").unwrap();
    let dropin = PluginId::parse("acme.dropin").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        official.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    state.plugins.insert(
        local.clone(),
        installed_record(PluginSource::Local, PluginInstallStatus::Ready),
    );
    let mut confirmed_drop_in = installed_record(PluginSource::DropIn, PluginInstallStatus::Ready);
    confirmed_drop_in.confirmed = true;
    state.plugins.insert(dropin.clone(), confirmed_drop_in);
    let plugins = [&official, &local, &dropin]
        .into_iter()
        .map(|id| {
            let mut plugin = catalog_plugin(id.as_str());
            plugin.versions[0].version = PluginVersion::parse("2.0.0").unwrap();
            plugin
        })
        .collect();
    let snapshot = catalog_snapshot(plugins);
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        assert_eq!(
            page.read(app)
                .update_candidates()
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            vec![official.clone()],
            "批量范围只能包含市场来源"
        );
        assert!(page.read(app).update_selection(&official).is_some());
        assert!(page.read(app).update_selection(&local).is_none());
        assert!(page.read(app).update_selection(&dropin).is_none());
        let rows = page.read(app).visible_rows(&snapshot.catalog.plugins, "");
        for row in rows {
            let id = row.selection.id();
            let has_action = row.action.is_some();
            assert_eq!(
                has_action,
                id == &official,
                "行内更新入口必须与批量资格一致: {}",
                id.as_str()
            );
        }
    });
}

/// 详情与批量共用资格：非市场来源不显示必然失败的更新按钮，而是说明原因。
#[gpui_kit::test]
fn detail_blocks_update_for_non_market_source(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.local").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Local, PluginInstallStatus::Ready),
    );
    let mut plugin = catalog_plugin(id.as_str());
    plugin.versions[0].version = PluginVersion::parse("2.0.0").unwrap();
    let snapshot = catalog_snapshot(vec![plugin]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.state = Some(state.clone());
            page.catalog = Some(snapshot.clone());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(id.clone()), cx)
        });
    });
    pump(&runtime, visual);
    assert!(
        visual.debug_bounds("extension-install").is_none(),
        "非市场来源不能显示必然失败的更新按钮"
    );
    assert!(
        visual.debug_bounds("extension-update-blocked").is_some(),
        "必须说明为何没有市场更新入口"
    );
}

/// 内置组标题是列表内第一个可聚焦控件；键盘 Tab 后按 Enter 必须能展开。
#[gpui_kit::test]
fn builtin_group_header_is_keyboard_reachable(app: &mut TestAppContext) {
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.builtin_units = vec![BuiltinUnit {
                id: PluginId::parse("kerminal.sftp").unwrap(),
                name: "SFTP".to_owned(),
                can_disable: true,
            }];
            page.state = Some(PluginStoreState::default());
            page.catalog = Some(empty_catalog_snapshot());
            page.builtin_expanded = false;
            page.active = true;
        });
        let search = page.read(app).search.clone();
        let handle = search.read(app).focus_handle(app);
        window.focus(&handle, app);
    });
    let mut on_list = false;
    for _ in 0..20 {
        visual.simulate_keystrokes("tab");
        on_list = visual.update(|window, app| page.read(app).list_focus.is_focused(window));
        if on_list {
            break;
        }
    }
    assert!(on_list, "Tab 必须先到达列表");
    let mut expanded = false;
    for _ in 0..4 {
        visual.simulate_keystrokes("tab");
        press_enter(visual);
        expanded = visual.update(|_, app| page.read(app).builtin_expanded);
        if expanded {
            break;
        }
    }
    assert!(expanded, "内置组标题必须能通过键盘展开");
}

/// 列表容器必须是真实 Tab 停靠点：从搜索出发只靠 Tab 序列就能到达列表。
#[gpui_kit::test]
fn tab_reaches_list_from_search(app: &mut TestAppContext) {
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        let search = page.read(app).search.clone();
        let handle = search.read(app).focus_handle(app);
        window.focus(&handle, app);
    });
    let mut reached = false;
    for _ in 0..20 {
        visual.simulate_keystrokes("tab");
        reached = visual.update(|window, app| page.read(app).list_focus.is_focused(window));
        if reached {
            break;
        }
    }
    assert!(reached, "从搜索按 Tab 必须能到达列表容器");
}

/// 行内按钮的 Enter 不能被列表导航吞掉；真实 Tab 聚焦按钮后应执行自身安装动作。
#[gpui_kit::test]
fn row_action_button_enter_executes_its_action(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.aa").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        let search = page.read(app).search.clone();
        let handle = search.read(app).focus_handle(app);
        window.focus(&handle, app);
    });
    // 先只按 Tab 到列表，避免在工具栏按钮上触发 Enter。
    let mut on_list = false;
    for _ in 0..20 {
        visual.simulate_keystrokes("tab");
        on_list = visual.update(|window, app| page.read(app).list_focus.is_focused(window));
        if on_list {
            break;
        }
    }
    assert!(on_list, "Tab 必须先到达列表");
    // 再 Tab 到行内按钮并按 Enter；按钮自己的动作应打开安装确认。
    let mut confirmed = false;
    for _ in 0..4 {
        visual.simulate_keystrokes("tab");
        press_enter(visual);
        confirmed = visual.update(|_, app| {
            let confirmation = page.read(app).confirmation.clone();
            matches!(
                confirmation,
                Some(Confirmation::Install(selection)) if selection.id == id
            )
        });
        if confirmed {
            break;
        }
    }
    assert!(confirmed, "行内按钮 Enter 必须执行自身安装动作");
    pump(&runtime, visual);
}

/// 行内安装按钮的点击不能冒泡成整行选择；窄窗里那会把列表换成详情。
#[gpui_kit::test]
fn row_action_click_does_not_open_detail(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.aa").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(id.as_str())]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        // 模拟宽窗键盘选择：高亮第一条但保留列表视图。
        page.update(app, |page, cx| {
            page.select_entry(SelectedEntry::Plugin(id.clone()), false, true, cx)
        });
    });
    // 元素注册表在绘制后才有行内按钮；先让上一步的更新完成自动重绘，再取真实布局。
    let bounds = visual.update(|window, _| {
        let selector: gpui_pre::ElementId = format!("extension-action-{}", id.as_str()).into();
        gpui_kit::base::test_support::find(window, &[], &selector)
            .expect("行内安装按钮应已渲染")
            .bounds()
    });
    visual.simulate_click(bounds.center(), Modifiers::default());
    visual.update(|_, app| {
        let page = page.read(app);
        assert!(page.back_to_list, "点击行内动作不能冒泡成整行选择");
        assert!(
            matches!(
                page.confirmation,
                Some(Confirmation::Install(ref selection)) if selection.id == id
            ),
            "行内按钮应打开自己的安装确认"
        );
    });
    pump(&runtime, visual);
}

/// 窄窗工具栏换行后搜索与“全部更新”仍必须可达；统一列表不再有模式切换。
#[gpui_kit::test]
fn narrow_toolbar_keeps_search_and_update_all_reachable(app: &mut TestAppContext) {
    let id = PluginId::parse("acme.notes").unwrap();
    let mut state = PluginStoreState::default();
    state.plugins.insert(
        id.clone(),
        installed_record(PluginSource::Official, PluginInstallStatus::Ready),
    );
    let mut plugin = catalog_plugin(id.as_str());
    plugin.versions[0].version = PluginVersion::parse("2.0.0").unwrap();
    let snapshot = catalog_snapshot(vec![plugin]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(state.clone(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(state.clone());
            page.active = true;
        });
    });
    visual.simulate_resize(size(px(400.), px(600.)));
    pump(&runtime, visual);
    assert!(
        visual.debug_bounds("extensions-search").is_some(),
        "窄窗搜索必须可达"
    );
    assert!(
        visual.debug_bounds("extensions-update-all").is_some(),
        "窄窗全部更新必须可达"
    );
}

/// 内置组标题是唯一折叠入口；箭头槽与行内容共用同一左 inset，搜索只临时展开。
#[gpui_kit::test]
fn builtin_group_header_toggles_collapse_and_aligns_arrow(app: &mut TestAppContext) {
    let unit = BuiltinUnit {
        id: PluginId::parse("kerminal.sftp").unwrap(),
        name: "SFTP".to_owned(),
        can_disable: true,
    };
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.builtin_units = vec![unit.clone()];
            page.state = Some(PluginStoreState::default());
            page.catalog = Some(empty_catalog_snapshot());
            page.active = true;
            page.builtin_expanded = false;
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert!(
            page.read(app).visible_rows(&[], "").is_empty(),
            "默认收起时内置行不显示"
        );
    });
    let arrow = visual
        .debug_bounds("extensions-group-arrow-builtin")
        .expect("内置组箭头槽应渲染");
    let header = visual
        .debug_bounds("extensions-group-builtin")
        .expect("内置组标题应可点击");
    visual.simulate_click(header.center(), Modifiers::default());
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert!(page.read(app).builtin_expanded, "点击标题必须展开内置组");
        assert_eq!(page.read(app).visible_rows(&[], "").len(), 1);
    });
    let content = visual
        .debug_bounds("extension-row-content-builtin-kerminal.sftp")
        .expect("内置行内容应渲染");
    assert!(
        (arrow.origin.x.as_f32() - content.origin.x.as_f32()).abs() < 0.5,
        "箭头槽与行内容左缘必须对齐: arrow={:?} content={:?}",
        arrow.origin,
        content.origin
    );
    // 搜索命中临时展开，但不改写用户的折叠选择。
    visual.update(|window, app| {
        page.update(app, |page, cx| {
            page.builtin_expanded = false;
            page.search.update(cx, |input, cx| {
                input.set_value("sftp".to_owned(), window, cx)
            });
        });
    });
    visual.update(|_, app| {
        assert!(!page.read(app).builtin_expanded, "搜索不能改写折叠状态");
        assert_eq!(
            page.read(app).visible_rows(&[], "sftp").len(),
            1,
            "搜索命中必须临时展示内置行"
        );
    });
}

/// 列表已可见时 Escape 不再被页面吞掉；外层监听器必须收到冒泡事件。
#[gpui_kit::test]
fn escape_in_visible_list_bubbles_to_parent(app: &mut TestAppContext) {
    let (runtime, page, escapes, visual) = probe_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.catalog = Some(empty_catalog_snapshot());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        let list_focus = page.read(app).list_focus.clone();
        window.focus(&list_focus, app);
    });
    visual.simulate_resize(size(px(400.), px(600.)));
    pump(&runtime, visual);
    visual.simulate_keystrokes("escape");
    visual.update(|_, app| {
        assert_eq!(escapes.get(), 1, "列表 Escape 必须冒泡到 Root");
        assert!(!page.read(app).back_to_list, "列表 Escape 不应伪造详情状态");
    });
}

/// 搜索隐藏旧高亮后 Enter 不能打开列表里看不到的项。
#[gpui_kit::test]
fn enter_ignores_selection_hidden_by_filter(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let second = PluginId::parse("acme.bb").unwrap();
    let snapshot = catalog_snapshot(vec![
        catalog_plugin(first.as_str()),
        catalog_plugin(second.as_str()),
    ]);
    let (_, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select_entry(SelectedEntry::Plugin(first.clone()), false, false, cx)
        });
        page.update(app, |page, cx| {
            page.search
                .update(cx, |input, cx| input.set_value("bb".to_owned(), window, cx));
        });
        let list_focus = page.read(app).list_focus.clone();
        window.focus(&list_focus, app);
    });
    visual.simulate_keystrokes("enter");
    visual.update(|_, app| {
        assert!(page.read(app).back_to_list, "Enter 不能打开被搜索筛掉的项");
        assert_eq!(
            page.read(app).selected,
            Some(SelectedEntry::Plugin(first.clone()))
        );
    });
}

/// 窄窗方向键高亮不读被遮住的详情；窗口变宽使详情可见后应补读一次。
#[gpui_kit::test]
fn hidden_highlight_reads_readme_when_detail_becomes_visible(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(first.as_str())]);
    let catalog = Arc::new(FakeCatalog::default());
    let calls = catalog.readme_calls.clone();
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog,
    );
    // 先以窄内容宽度进入首帧，避免夹具默认 900px 的上一帧测量把详情误判为可见，
    // 让本用例只验证“窄窗高亮不读、变宽后补读”这一条路径。
    visual.simulate_resize(size(px(400.), px(600.)));
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.content_width.set(Some(px(100.)));
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select_entry(SelectedEntry::Plugin(first.clone()), false, false, cx)
        });
    });
    pump(&runtime, visual);
    visual.update(|_, _| {
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "窄窗列表高亮不读被遮住的详情"
        );
    });
    visual.simulate_resize(size(px(900.), px(600.)));
    // 变宽后首帧仍按上一帧窄宽度渲染，测量回写并再绘一帧后才会补读；
    // 多推进几轮 GPUI/Tokio，断言只依赖最终可观察结果。
    for _ in 0..8 {
        pump(&runtime, visual);
        if calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
    }
    visual.update(|_, _| {
        assert_eq!(calls.load(Ordering::SeqCst), 1, "详情可见后应补读 README");
    });
}

/// 离开分类必须取消 README 并复位去重；重新进入且详情可见时应再次读取。
#[gpui_kit::test]
fn deactivate_cancels_readme_and_reentry_reads_again(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(first.as_str())]);
    let catalog = Arc::new(FakeCatalog::default());
    let (gate_tx, gate_rx) = oneshot::channel();
    *catalog.readme_gate.lock().unwrap() = Some(gate_rx);
    let calls = catalog.readme_calls.clone();
    let tokens = catalog.readme_tokens.clone();
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        catalog,
    );
    visual.update(|_, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(first.clone()), cx)
        });
    });
    pump(&runtime, visual);
    // 必须在同一个窗口更新闭包内断言离开分类的同步效果：测试模式下每次更新结束
    // 都会自动重绘脏窗口，render 会把 active 置真并按需补读，晚一步就看不到复位。
    let token = visual.update(|_, app| {
        let page_token = page
            .read(app)
            .readme_cancel
            .clone()
            .expect("页面持有取消令牌");
        let service_token = tokens
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("README 已发起");
        page.update(app, |page, cx| page.deactivate(cx));
        assert!(
            page_token.is_cancelled(),
            "离开分类必须取消页面 README 令牌"
        );
        let page = page.read(app);
        assert!(!page.readme_attempted, "取消后必须复位去重标记");
        assert!(page.readme_cancel.is_none());
        assert!(!page.active);
        service_token
    });
    // 执行器在下一个任务轮次才把调用方取消转发给业务子令牌；只推进 Tokio 队列，
    // 不额外触发窗口更新。
    for _ in 0..10 {
        runtime.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        });
    }
    assert!(token.is_cancelled(), "取消必须传播到业务请求");
    // 重绘会重新激活页面并按需补读；pump 推进异步回执。
    pump(&runtime, visual);
    visual.update(|_, _| {
        assert!(calls.load(Ordering::SeqCst) >= 2, "重入后应重新读取 README");
    });
    let _ = gate_tx.send(());
    pump(&runtime, visual);
}

/// 宽窗列表持焦后缩窄会卸载列表；焦点必须交给可见详情，反向切回列表也对称。
#[gpui_kit::test]
fn focus_hands_over_when_layout_swaps_mounted_surface(app: &mut TestAppContext) {
    let first = PluginId::parse("acme.aa").unwrap();
    let snapshot = catalog_snapshot(vec![catalog_plugin(first.as_str())]);
    let (runtime, page, visual) = page_fixture(
        app,
        Arc::new(FakeStore::new(PluginStoreState::default(), Vec::new())),
        Arc::new(FakeCatalog::default()),
    );
    visual.update(|window, app| {
        page.update(app, |page, _| {
            page.catalog = Some(snapshot.clone());
            page.state = Some(PluginStoreState::default());
            page.active = true;
        });
        page.update(app, |page, cx| {
            page.select(SelectedEntry::Plugin(first.clone()), cx)
        });
        let list_focus = page.read(app).list_focus.clone();
        window.focus(&list_focus, app);
    });
    pump(&runtime, visual);
    // 宽窗列表持焦，缩窄后列表卸载、详情可见，焦点应交给详情。
    visual.simulate_resize(size(px(400.), px(600.)));
    pump(&runtime, visual);
    visual.update(|window, app| {
        assert!(
            page.read(app).detail_focus.is_focused(window),
            "缩窄后焦点应交给详情"
        );
        assert!(!page.read(app).list_focus.is_focused(window));
    });
    // 返回列表使详情卸载，焦点应回到列表。
    visual.update(|_, app| {
        page.update(app, |page, _| page.back_to_list = true);
    });
    pump(&runtime, visual);
    visual.update(|window, app| {
        assert!(
            page.read(app).list_focus.is_focused(window),
            "返回列表后焦点应回到列表"
        );
        assert!(!page.read(app).detail_focus.is_focused(window));
    });
}

/// 测试与验收读取当前生效来源；运行态只能经设置保存回执切换，不暴露可变入口。
///
/// 访问器只服务断言，放在测试模块里，生产源码不必为测试构建保留入口。
///
/// @author kongweiguang
impl ExtensionsPage {
    pub(crate) fn current_source_url(&self) -> String {
        self.service.source_url()
    }

    /// 测试与验收读取来源保存是否仍在等待回执；运行态不暴露可变入口。
    ///
    /// @author kongweiguang
    pub(crate) fn catalog_source_saving(&self) -> bool {
        self.source_saving
    }

    /// 测试与验收读取来源保存失败原因；没有错误时返回 None。
    ///
    /// @author kongweiguang
    pub(crate) fn catalog_source_error(&self) -> Option<&str> {
        self.source_error.as_deref()
    }

    /// 测试从真实页面入口提交来源，确保面板保存路径与页面待确认状态一致。
    ///
    /// 只用于 UI 测试：生产路径必须由用户点击保存按钮触发，不能绕过忙碌门禁。
    ///
    /// @author kongweiguang
    pub(crate) fn submit_source_url_for_test(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.source_input
            .update(cx, |input, cx| input.set_value(url, window, cx));
        self.request_source_save(cx);
    }

    /// 可见行投影保留给测试断言；生产渲染和键盘导航使用完整条目序列。
    ///
    /// 生产路径不再消费它，避免两套索引再次分叉，因此实现留在测试模块，
    /// 源码只保留 `list_entries` 这一份真实投影。
    ///
    /// @author kongweiguang
    fn visible_rows(&self, plugins: &[CatalogPlugin], query: &str) -> Vec<ExtensionRow> {
        self.list_entries(plugins, query)
            .into_iter()
            .filter_map(|entry| match entry {
                ListEntry::Row(row) => Some(*row),
                ListEntry::Header { .. } => None,
            })
            .collect()
    }
}

/// 页头总数按稳定身份去重，目录与内置名单出现同名身份时只算一个。
///
/// 双模式改造后页头分别展示目录行数与已安装数，生产渲染不再需要合并计数；
/// 该约定只由测试固定，因此实现放在测试模块，避免源码出现测试专用函数。
///
/// @author kongweiguang
fn extension_total(plugins: &[CatalogPlugin], units: &[BuiltinUnit]) -> usize {
    let mut ids: BTreeSet<&str> = plugins.iter().map(|plugin| plugin.id.as_str()).collect();
    ids.extend(units.iter().map(|unit| unit.id.as_str()));
    ids.len()
}
