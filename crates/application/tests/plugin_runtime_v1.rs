//! 插件运行服务的身份、权限与崩溃恢复回归。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    plugin_ports::{
        PluginHostApi, PluginHostError, PluginHostRouter, PluginProcessMessage, PluginProcessPort,
        PluginRuntime, PluginRuntimeEvent, PluginViewIdentity, PluginViewSavedState,
        PreparedPlugin,
    },
    plugin_runtime::{PluginRuntimeDescriptor, PluginRuntimeService},
};
use kerminal_domain::plugins::{PluginId, PluginRuntimeState, PluginVersion};
use kerminal_plugin_protocol::{
    ManifestPolicy, Permission, ValidatedPackage, error_code, parse_manifest,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct FakeProcess {
    state: Mutex<PluginRuntimeState>,
    router: Mutex<Option<Weak<dyn PluginHostRouter>>>,
    calls: Mutex<Vec<(String, Value)>>,
    activations: AtomicUsize,
    close_times_out: AtomicBool,
    fail_reopen: AtomicBool,
    oversized_reopen: AtomicBool,
    pause_reopen: AtomicBool,
    reopen_entered: tokio::sync::Semaphore,
    reopen_release: tokio::sync::Semaphore,
    events: broadcast::Sender<PluginProcessMessage>,
}

impl FakeProcess {
    /// 模拟进程只暴露测试关心的状态，记录 wire 身份以验证隔离。
    fn new() -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            state: Mutex::new(PluginRuntimeState::Ready { generation: 1 }),
            router: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            activations: AtomicUsize::new(0),
            close_times_out: AtomicBool::new(false),
            fail_reopen: AtomicBool::new(false),
            oversized_reopen: AtomicBool::new(false),
            pause_reopen: AtomicBool::new(false),
            reopen_entered: tokio::sync::Semaphore::new(0),
            reopen_release: tokio::sync::Semaphore::new(0),
            events,
        }
    }

    /// 进程事件与同步状态一起更新，和真实监督器保持相同的发布顺序。
    fn publish(&self, plugin: &PluginId, generation: u64, state: PluginRuntimeState, label: &str) {
        *self.state.lock().expect("state lock") = state;
        let _ = self.events.send(PluginProcessMessage {
            plugin: plugin.clone(),
            generation,
            method: "$/process-state".into(),
            params: json!({"state":label}),
        });
    }

    /// 提取发送给指定视图的最新 wire 身份，不从公开实例 ID 猜测。
    fn wire(&self, view: &str) -> String {
        self.calls
            .lock()
            .expect("calls lock")
            .iter()
            .rev()
            .find(|(method, params)| method == "view.open" && params["view"] == view)
            .and_then(|(_, params)| params["instance"].as_str())
            .expect("open wire")
            .to_owned()
    }
}

#[async_trait]
impl PluginProcessPort for FakeProcess {
    /// 记录弱引用以模拟同一个权限路由，不延长应用服务生命周期。
    fn register_host_router(&self, router: Weak<dyn PluginHostRouter>) -> AppResult<()> {
        *self.router.lock().expect("router lock") = Some(router);
        Ok(())
    }
    /// 状态由测试显式改变，避免假装进程总是就绪。
    fn state(&self, _: &PluginId) -> AppResult<PluginRuntimeState> {
        Ok(self.state.lock().expect("state lock").clone())
    }
    /// 测试只在 Ready 状态返回当前代次。
    async fn activate(&self, _: &PluginId, _: Instant) -> AppResult<u64> {
        self.activations.fetch_add(1, Ordering::Relaxed);
        match *self.state.lock().expect("state lock") {
            PluginRuntimeState::Ready { generation } => Ok(generation),
            _ => Err(AppError::conflict("进程不可用")),
        }
    }
    /// 测试显式重试沿用已就绪代次，不假造新的进程。
    async fn retry(&self, plugin: &PluginId, deadline: Instant) -> AppResult<u64> {
        self.activate(plugin, deadline).await
    }
    /// 通过记录参数验证服务没有把不同实例合并。
    async fn call(
        &self,
        _: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        _: Instant,
        _: CancellationToken,
    ) -> AppResult<Value> {
        if self.state(&PluginId::parse("acme.notes").expect("id"))?
            != (PluginRuntimeState::Ready { generation })
        {
            return Err(AppError::conflict("旧代次"));
        }
        self.calls
            .lock()
            .expect("calls lock")
            .push((method.into(), params));
        if method == "view.open" && generation > 1 {
            if self.pause_reopen.load(Ordering::SeqCst) {
                self.reopen_entered.add_permits(1);
                self.reopen_release
                    .acquire()
                    .await
                    .expect("release open")
                    .forget();
            }
            if self.fail_reopen.swap(false, Ordering::SeqCst) {
                return Err(AppError::conflict("模拟恢复失败"));
            }
            if self.oversized_reopen.swap(false, Ordering::SeqCst) {
                return Ok(
                    json!({"data":{"large":"x".repeat(kerminal_plugin_protocol::MAX_VIEW_BYTES + 1)}}),
                );
            }
        }
        if method == "view.close" && self.close_times_out.load(Ordering::Relaxed) {
            return Err(AppError::timeout("插件视图关闭"));
        }
        Ok(match method {
            "view.snapshot" | "view.close" => json!({"state_version":1,"state":{"draft":"saved"}}),
            "view.open" => json!({"data":{"ready":true}}),
            "view.action" => json!({"patch":{"counter":2}}),
            _ => json!({}),
        })
    }
    /// 可见性通知不改变测试中的视图状态。
    async fn notify(&self, _: &PluginId, _: u64, _: &str, _: Value, _: Instant) -> AppResult<()> {
        Ok(())
    }
    /// 监听者直接订阅模拟进程的状态事件。
    fn subscribe(&self) -> broadcast::Receiver<PluginProcessMessage> {
        self.events.subscribe()
    }
    /// 模拟器不拥有实际系统资源。
    async fn shutdown_until(&self, _: Instant) -> AppResult<()> {
        Ok(())
    }
}

struct FakeHost;

#[async_trait]
impl PluginHostApi for FakeHost {
    /// 已授权请求返回固定结果，便于区分路由拒绝和后端调用。
    async fn invoke(
        &self,
        _: &PluginId,
        _: u64,
        _: Option<&PluginViewIdentity>,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        Ok(json!({"accepted":true}))
    }
}

/// 每个用例创建隔离服务和共享执行器，不依赖桌面或用户配置。
fn fixture(
    permissions: BTreeSet<Permission>,
) -> (Arc<PluginRuntimeService>, Arc<FakeProcess>, PluginId) {
    fixture_with_runtime(permissions, false)
}

/// 对照纯声明插件与脚本插件，避免把进程激活错误套到全部视图。
fn fixture_with_runtime(
    permissions: BTreeSet<Permission>,
    declarative: bool,
) -> (Arc<PluginRuntimeService>, Arc<FakeProcess>, PluginId) {
    let plugin = PluginId::parse("acme.notes").expect("id");
    let process = Arc::new(FakeProcess::new());
    let manifest = json!({
        "schema":1,"id":"acme.notes","name":{"en":"Notes"},"description":{"en":"Notes"},
        "version":"1.0.0","api":"^1.0","publisher":"acme",
        "runtime":if declarative { json!({"kind":"none"}) } else { json!({"kind":"script","main":"main.js"}) },
        "permissions":permissions.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "views":{"notes.editor":{"kind":"native","file":"views/editor.json"}}
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&manifest).expect("manifest JSON"),
        ManifestPolicy::ThirdParty,
    )
    .expect("manifest");
    let prepared = Arc::new(PreparedPlugin {
        id: plugin.clone(),
        version: PluginVersion::parse("1.0.0").expect("version"),
        directory: PathBuf::from("fixture"),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::from([(
            "notes.editor".into(),
            Arc::new(json!({"data":{"nested":{"first":1,"deleted":true},"initial":true}})),
        )]),
        localizations: BTreeMap::new(),
        resources: BTreeMap::new(),
    });
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let service = PluginRuntimeService::new(
        vec![PluginRuntimeDescriptor {
            prepared,
            permissions,
        }],
        process.clone(),
        Arc::new(FakeHost),
        executor,
    )
    .expect("service");
    (service, process, plugin)
}

#[tokio::test]
/// 声明式插件无需进程也能打开界面，并且宿主能力仍需通过权限校验。
async fn declarative_views_do_not_activate_a_process() {
    let (service, process, plugin) =
        fixture_with_runtime(BTreeSet::from([Permission::UiNotify]), true);
    let instance = identity(&plugin, "notes.editor", 1);
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    assert!(process.calls.lock().expect("calls lock").is_empty());
    assert_eq!(process.activations.load(Ordering::Relaxed), 0);
    assert_eq!(
        service.process_generation(&plugin).expect("generation"),
        Some(0)
    );
    assert_eq!(
        service.cached_data(&instance).expect("cache").unwrap()["initial"],
        true
    );
    let result = service
        .route_webview(
            &instance,
            "ui.notify",
            json!({"message":"hello"}),
            CancellationToken::new(),
        )
        .await
        .expect("permitted");
    assert_eq!(result["accepted"], true);
    service.close(&instance, deadline()).await.expect("close");
}

/// 构造实例时同时变化宿主代次，模拟用户关闭后重新打开同一个 Tab。
fn identity(plugin: &PluginId, view: &str, generation: u64) -> PluginViewIdentity {
    PluginViewIdentity {
        plugin: plugin.clone(),
        view: view.into(),
        instance: "tab-1".into(),
        generation,
    }
}

/// 绝对截止时间覆盖整个测试调用，防止失败时无限等待。
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}

#[tokio::test]
/// 关闭重开后旧 wire 身份必须被拒绝，即使插件与业务实例名称相同。
async fn reopened_view_rejects_old_wire_identity() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let first = identity(&plugin, "notes.editor", 1);
    service
        .open(
            first.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let old_wire = process.wire("notes.editor");
    service.close(&first, deadline()).await.expect("close");
    let second = identity(&plugin, "notes.editor", 2);
    service
        .open(
            second,
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("reopen");
    assert_ne!(old_wire, process.wire("notes.editor"));
    let result = service
        .route(
            &plugin,
            1,
            "view.update",
            json!({"instance":old_wire,"patch":{"draft":"late"}}),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        result.expect_err("old message rejected").code,
        error_code::STALE
    );
}

#[tokio::test]
/// WebView 和进程调用必须采用同一权限判断与实例门禁。
async fn webview_and_process_share_permission_gate() {
    let (service, process, plugin) = fixture(BTreeSet::from([Permission::UiNotify]));
    let instance = identity(&plugin, "notes.editor", 1);
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let wire = process.wire("notes.editor");
    let denied = service
        .route_webview(&instance, "ui.prompt", json!({}), CancellationToken::new())
        .await
        .expect_err("prompt denied");
    assert_eq!(denied.code, error_code::PERMISSION_DENIED);
    let result = service
        .route(
            &plugin,
            1,
            "ui.notify",
            json!({"instance":wire,"message":"hi"}),
            CancellationToken::new(),
        )
        .await
        .expect("notification allowed");
    assert_eq!(result["accepted"], true);
    let unknown = service
        .route_webview(
            &instance,
            "arbitrary.operation",
            json!({}),
            CancellationToken::new(),
        )
        .await
        .expect_err("unknown denied");
    assert_eq!(unknown.code, error_code::METHOD_NOT_FOUND);
}

#[tokio::test]
/// 崩溃后以最新已保存状态重新打开，并换用新的进程与 wire 代次。
async fn restart_reopens_instances_with_saved_state() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    service
        .open(
            instance.clone(),
            json!({"tab":"one"}),
            Some(PluginViewSavedState {
                state_version: 1,
                state: json!({"draft":"original"}),
            }),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    service
        .snapshot(&instance, deadline(), CancellationToken::new())
        .await
        .expect("snapshot");
    let old_wire = process.wire("notes.editor");
    let mut events = service.subscribe();
    let old_revision = service
        .cached_data_snapshot(&instance)
        .expect("snapshot")
        .unwrap()
        .revision;
    process.publish(
        &plugin,
        1,
        PluginRuntimeState::Crashed { generation: 1 },
        "crashed",
    );
    process.publish(
        &plugin,
        2,
        PluginRuntimeState::Ready { generation: 2 },
        "ready",
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if process.wire("notes.editor") != old_wire
                && service
                    .cached_data_snapshot(&instance)
                    .expect("snapshot")
                    .unwrap()
                    .revision
                    > old_revision
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reopen observed");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(events.recv().await.expect("event"),
                PluginRuntimeEvent::ViewReopened { identity, process_generation: 2 } if identity == instance)
            {
                break;
            }
        }
    }).await.expect("reopen confirmation delivered");
    assert_eq!(
        service
            .cached_data_snapshot(&instance)
            .expect("snapshot")
            .unwrap()
            .revision,
        old_revision + 1
    );
    let calls = process.calls.lock().expect("calls lock");
    let (_, reopened) = calls
        .iter()
        .rev()
        .find(|(method, _)| method == "view.open")
        .expect("reopened call");
    assert_eq!(reopened["state"]["draft"], "saved");
    assert_ne!(reopened["instance"], old_wire);
}

#[tokio::test]
/// 权威缓存按合并补丁保存进程结果，事件始终携带原始进程代次。
async fn view_data_cache_applies_merge_patches_and_tracks_generation() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    let mut events = service.subscribe();
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    assert_eq!(
        service.process_generation(&plugin).expect("generation"),
        Some(1)
    );
    assert_eq!(
        service.cached_data(&instance).expect("cache").unwrap()["ready"],
        true
    );
    let patch = json!({"nested":{"first":2,"deleted":null},"ready":null});
    service
        .route(
            &plugin,
            1,
            "view.update",
            json!({"instance":process.wire("notes.editor"),"patch":patch}),
            CancellationToken::new(),
        )
        .await
        .expect("update");
    assert_eq!(
        service.cached_data(&instance).expect("cache"),
        Some(json!({"nested":{"first":2},"initial":true}))
    );
    loop {
        match events.recv().await.expect("event") {
            PluginRuntimeEvent::ViewUpdate {
                identity,
                process_generation,
                revision,
                patch: actual,
            } if actual == patch => {
                assert_eq!(identity, instance);
                assert_eq!(process_generation, 1);
                assert_eq!(revision, 2);
                break;
            }
            _ => continue,
        }
    }
    let old = service.cached_data(&instance).expect("cache");
    let oversized = "x".repeat(kerminal_plugin_protocol::MAX_VIEW_PATCH_BYTES + 1);
    assert_eq!(
        service
            .route(
                &plugin,
                1,
                "view.update",
                json!({"instance":process.wire("notes.editor"),"patch":{"value":oversized}}),
                CancellationToken::new()
            )
            .await
            .expect_err("oversized patch")
            .code,
        error_code::INVALID_PARAMS
    );
    assert_eq!(service.cached_data(&instance).expect("cache"), old);
}

#[tokio::test]
/// 声明式视图只回读已有恢复状态，未知状态不能伪造为有效快照。
async fn declarative_snapshot_and_close_preserve_saved_state() {
    let (service, process, plugin) = fixture_with_runtime(BTreeSet::new(), true);
    let instance = identity(&plugin, "notes.editor", 1);
    let saved = PluginViewSavedState {
        state_version: 2,
        state: json!({"draft":"saved"}),
    };
    service
        .open(
            instance.clone(),
            json!({}),
            Some(saved.clone()),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    assert_eq!(
        service
            .snapshot(&instance, deadline(), CancellationToken::new())
            .await
            .expect("snapshot"),
        saved
    );
    assert_eq!(
        service.close(&instance, deadline()).await.expect("close"),
        Some(saved)
    );
    assert_eq!(process.activations.load(Ordering::Relaxed), 0);
}

#[tokio::test]
/// 关闭超时仍应释放宿主身份并暴露失败，迟到的插件更新不能命中关闭后的视图。
async fn timed_out_close_reclaims_instance_without_faking_success() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    let saved = PluginViewSavedState {
        state_version: 1,
        state: json!({"draft":"kept"}),
    };
    let mut events = service.subscribe();
    service
        .open(
            instance.clone(),
            json!({}),
            Some(saved.clone()),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let wire = process.wire("notes.editor");
    process.close_times_out.store(true, Ordering::Relaxed);
    assert!(matches!(
        service.close(&instance, deadline()).await,
        Err(AppError::Timeout { .. })
    ));
    assert_eq!(service.cached_data(&instance).expect("cache"), None);
    assert_eq!(
        service
            .route(
                &plugin,
                1,
                "view.update",
                json!({"instance":wire,"patch":{"late":true}}),
                CancellationToken::new()
            )
            .await
            .expect_err("closed identity")
            .code,
        error_code::STALE
    );
    loop {
        if let PluginRuntimeEvent::ViewClosed { identity, state } =
            events.recv().await.expect("event")
        {
            assert_eq!(identity, instance);
            assert_eq!(state, Some(saved));
            break;
        }
    }
}

#[tokio::test]
/// 崩溃后的窗口必须能释放实例并保留最后确认状态，后续重启不能复活已关闭视图。
async fn crashed_view_closes_without_reopening() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let expected = service
        .snapshot(&instance, deadline(), CancellationToken::new())
        .await
        .expect("snapshot");
    let mut events = service.subscribe();
    process.publish(
        &plugin,
        1,
        PluginRuntimeState::Crashed { generation: 1 },
        "crashed",
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                events.recv().await.expect("process state"),
                PluginRuntimeEvent::ProcessState {
                    state: PluginRuntimeState::Crashed { .. },
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("crash delivered");
    assert_eq!(
        service.close(&instance, deadline()).await.expect("close"),
        Some(expected)
    );
    process.publish(
        &plugin,
        2,
        PluginRuntimeState::Ready { generation: 2 },
        "ready",
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                events.recv().await.expect("process state"),
                PluginRuntimeEvent::ProcessState {
                    state: PluginRuntimeState::Ready { generation: 2 },
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("restart delivered");
    assert_eq!(
        process
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|(method, _)| method == "view.open")
            .count(),
        1
    );
    assert!(service.cached_state(&instance).is_err());
}
#[tokio::test]
/// 视图身份必须属于冻结清单，未知视图不得触发插件进程。
async fn undeclared_view_is_rejected_before_activation() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let unknown = identity(&plugin, "notes.unknown", 1);
    assert!(
        service
            .open(
                unknown,
                json!({}),
                None,
                deadline(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(process.activations.load(Ordering::Relaxed), 0);
}

#[tokio::test]
/// 快照水位覆盖已排队的旧补丁，之后的动作和更新仍保持单调顺序。
async fn snapshot_revision_filters_buffered_patches_and_preserves_later_updates() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    let mut events = service.subscribe();
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let wire = process.wire("notes.editor");
    service
        .ordered_view_update(&plugin, 1, json!({"instance":wire,"patch":{"counter":1}}))
        .expect("first update");
    service
        .invoke(
            &instance,
            "view.action",
            json!({"action":"set"}),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("action");
    let snapshot = service
        .cached_data_snapshot(&instance)
        .expect("snapshot read")
        .expect("instance");
    assert_eq!(snapshot.revision, 3);
    assert_eq!(snapshot.data["counter"], 2);
    service
        .ordered_view_update(&plugin, 1, json!({"instance":wire,"patch":{"counter":3}}))
        .expect("later update");
    let mut recovered = snapshot.data;
    let mut revisions = Vec::new();
    while revisions.len() < 4 {
        if let PluginRuntimeEvent::ViewUpdate {
            revision, patch, ..
        } = events.recv().await.expect("event")
        {
            revisions.push(revision);
            if revision > snapshot.revision {
                for (key, value) in patch.as_object().expect("test patch object") {
                    recovered[key] = value.clone();
                }
            }
        }
    }
    assert_eq!(revisions, [1, 2, 3, 4]);
    assert_eq!(
        recovered,
        service
            .cached_data(&instance)
            .expect("authoritative cache")
            .unwrap()
    );
    assert_eq!(recovered["counter"], 3);
}

#[tokio::test]
/// 恢复失败只影响对应实例，进程仍就绪时用户可显式重试并收到成功确认。
async fn failed_reopen_can_be_explicitly_retried_with_ready_process() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let mut events = service.subscribe();
    process.fail_reopen.store(true, Ordering::SeqCst);
    process.publish(
        &plugin,
        1,
        PluginRuntimeState::Crashed { generation: 1 },
        "crashed",
    );
    process.publish(
        &plugin,
        2,
        PluginRuntimeState::Ready { generation: 2 },
        "ready",
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(events.recv().await.expect("event"), PluginRuntimeEvent::ViewReopenFailed { identity, process_generation: 2, .. } if identity == instance) { break; }
        }
    }).await.expect("failure reported");
    assert_eq!(
        service
            .retry_reopen(instance.clone(), deadline(), CancellationToken::new())
            .await
            .expect("retry"),
        2
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(events.recv().await.expect("event"), PluginRuntimeEvent::ViewReopened { identity, process_generation: 2 } if identity == instance) { break; }
        }
    }).await.expect("retry confirmed");
    let calls = process.calls.lock().expect("calls");
    let methods = calls
        .iter()
        .map(|(method, _)| method.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        vec!["view.open", "view.open", "view.close", "view.open"]
    );
    assert_eq!(calls[1].1["instance"], calls[2].1["instance"]);
    assert_ne!(calls[1].1["instance"], calls[3].1["instance"]);
}

#[tokio::test]
/// 恢复请求已发送时关闭需等待其结束，保证插件端不会出现无人持有的实例。
async fn close_waits_for_inflight_automatic_reopen() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    process.pause_reopen.store(true, Ordering::SeqCst);
    process.publish(
        &plugin,
        1,
        PluginRuntimeState::Crashed { generation: 1 },
        "crashed",
    );
    process.publish(
        &plugin,
        2,
        PluginRuntimeState::Ready { generation: 2 },
        "ready",
    );
    tokio::time::timeout(Duration::from_secs(2), process.reopen_entered.acquire())
        .await
        .expect("reopen entered")
        .expect("semaphore")
        .forget();
    let closing_service = service.clone();
    let closing_identity = instance.clone();
    let mut closing =
        tokio::spawn(async move { closing_service.close(&closing_identity, deadline()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut closing)
            .await
            .is_err()
    );
    process.reopen_release.add_permits(1);
    closing.await.expect("close task").expect("close");
    assert!(service.cached_state(&instance).is_err());
    let calls = process.calls.lock().expect("calls");
    let methods = calls
        .iter()
        .map(|(method, _)| method.as_str())
        .collect::<Vec<_>>();
    assert_eq!(methods, vec!["view.open", "view.open", "view.close"]);
}

#[tokio::test]
/// 旧 wire 的关闭无确认时不得重开新实例；随后明确重试仍能回收并恢复。
async fn failed_reopen_retains_wire_until_cleanup_acknowledged() {
    let (service, process, plugin) = fixture(BTreeSet::new());
    let instance = identity(&plugin, "notes.editor", 1);
    service
        .open(
            instance.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let mut events = service.subscribe();
    process.oversized_reopen.store(true, Ordering::SeqCst);
    process.publish(
        &plugin,
        1,
        PluginRuntimeState::Crashed { generation: 1 },
        "crashed",
    );
    process.publish(
        &plugin,
        2,
        PluginRuntimeState::Ready { generation: 2 },
        "ready",
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(events.recv().await.expect("event"), PluginRuntimeEvent::ViewReopenFailed { identity, process_generation: 2, .. } if identity == instance) { break; }
        }
    }).await.expect("failure reported");
    let failed_wire = process.wire("notes.editor");
    process.close_times_out.store(true, Ordering::SeqCst);
    assert!(
        service
            .retry_reopen(instance.clone(), deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    {
        let calls = process.calls.lock().expect("calls");
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "view.open")
                .count(),
            2
        );
        assert_eq!(calls.last().expect("close").1["instance"], failed_wire);
    }
    assert!(service.close(&instance, deadline()).await.is_err());
    assert!(service.cached_state(&instance).is_ok());
    process.close_times_out.store(false, Ordering::SeqCst);
    service
        .retry_reopen(instance.clone(), deadline(), CancellationToken::new())
        .await
        .expect("cleanup and retry");
    let calls = process.calls.lock().expect("calls");
    let closes = calls
        .iter()
        .filter(|(method, _)| method == "view.close")
        .collect::<Vec<_>>();
    assert_eq!(closes.len(), 3);
    assert!(
        closes
            .iter()
            .all(|close| close.1["instance"] == failed_wire)
    );
    assert_ne!(calls.last().expect("new open").1["instance"], failed_wire);
}
