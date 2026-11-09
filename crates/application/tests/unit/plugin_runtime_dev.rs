//! 开发资源换代与实例关闭竞态回归。
//! @author kongweiguang

use std::sync::{Weak, atomic::AtomicUsize};

use crate::plugin_ports::PluginProcessMessage;
use kerminal_domain::plugins::PluginVersion;
use kerminal_plugin_protocol::{ManifestPolicy, ValidatedPackage, parse_manifest};
use tokio::sync::Notify;

use super::*;

struct TestProcess {
    states: Mutex<BTreeMap<PluginId, u64>>,
    calls: Mutex<Vec<(PluginId, u64, String, Value)>>,
    restarts: AtomicUsize,
    pause_restart: AtomicBool,
    restart_entered: Notify,
    restart_release: Notify,
    pause_close: AtomicBool,
    close_entered: Notify,
    close_release: Notify,
    events: broadcast::Sender<PluginProcessMessage>,
}

impl TestProcess {
    /// 测试进程保留每个插件自己的代次，以发现意外重启所有插件的实现。
    fn new() -> Self {
        Self {
            states: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            restarts: AtomicUsize::new(0),
            pause_restart: AtomicBool::new(false),
            restart_entered: Notify::new(),
            restart_release: Notify::new(),
            pause_close: AtomicBool::new(false),
            close_entered: Notify::new(),
            close_release: Notify::new(),
            events: broadcast::channel(32).0,
        }
    }
}

#[async_trait]
impl PluginProcessPort for TestProcess {
    /// 路由本身不参与本组测试，不额外保留服务所有权。
    fn register_host_router(&self, _: Weak<dyn PluginHostRouter>) -> AppResult<()> {
        Ok(())
    }

    /// 状态由进程模拟器的代次决定，不能将其他插件的 ready 状态共用。
    fn state(&self, plugin: &PluginId) -> AppResult<PluginRuntimeState> {
        Ok(PluginRuntimeState::Ready {
            generation: *self
                .states
                .lock()
                .expect("states")
                .get(plugin)
                .unwrap_or(&1),
        })
    }

    /// 初次打开不触发重启，仅返回该插件当前代次。
    async fn activate(&self, plugin: &PluginId, _: Instant) -> AppResult<u64> {
        Ok(*self
            .states
            .lock()
            .expect("states")
            .entry(plugin.clone())
            .or_insert(1))
    }

    /// 在可控屏障内模拟定向停止，便于在失联窗口同时关闭宿主视图。
    async fn restart(&self, plugin: &PluginId, _: Instant) -> AppResult<u64> {
        self.restarts.fetch_add(1, Ordering::SeqCst);
        if self.pause_restart.load(Ordering::SeqCst) {
            self.restart_entered.notify_one();
            self.restart_release.notified().await;
        }
        let generation = {
            let mut states = self.states.lock().expect("states");
            let generation = states.entry(plugin.clone()).or_insert(1);
            *generation += 1;
            *generation
        };
        let _ = self.events.send(PluginProcessMessage {
            plugin: plugin.clone(),
            generation,
            method: "$/process-state".into(),
            params: json!({"state":"ready"}),
        });
        Ok(generation)
    }

    /// 空打开回执用于验证恢复不依赖插件恰好返回 data 字段。
    async fn call(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        _: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("已取消"));
        }
        self.calls
            .lock()
            .expect("calls")
            .push((plugin.clone(), generation, method.into(), params));
        if method == "view.close" && self.pause_close.load(Ordering::SeqCst) {
            self.close_entered.notify_one();
            self.close_release.notified().await;
        }
        Ok(json!({}))
    }

    /// 显隐通知不会改变测试所需的打开调用计数。
    async fn notify(&self, _: &PluginId, _: u64, _: &str, _: Value, _: Instant) -> AppResult<()> {
        Ok(())
    }

    /// 使用真实广播队列，覆盖 ready 事件与显式重开之间的协调。
    fn subscribe(&self) -> broadcast::Receiver<PluginProcessMessage> {
        self.events.subscribe()
    }

    /// 本测试不启动 OS 进程，关闭没有额外资源。
    async fn shutdown_until(&self, _: Instant) -> AppResult<()> {
        Ok(())
    }
}

struct TestHost;

#[async_trait]
impl PluginHostApi for TestHost {
    /// 热重载不应调用宿主副作用，意外调用直接失败。
    async fn invoke(
        &self,
        _: &PluginId,
        _: u64,
        _: Option<&PluginViewIdentity>,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        Err(host_error(error_code::METHOD_NOT_FOUND, "unexpected"))
    }
}

/// 小型真实协议快照省去文件系统，让测试只约束生命周期与资源换代。
fn prepared(id: &str, declarative: bool) -> Arc<PreparedPlugin> {
    let manifest = parse_manifest(&serde_json::to_vec(&json!({
        "schema":1,"id":id,"version":"1.0.0","api":"^1.0","publisher":"acme",
        "name":{"en":"Notes"},"description":{"en":"Notes"},
        "runtime":if declarative { json!({"kind":"none"}) } else { json!({"kind":"script","main":"main.js"}) },
        "views":{"editor":{"kind":"native","file":"editor.json"}}
    })).expect("json"), ManifestPolicy::ThirdParty).expect("manifest");
    Arc::new(PreparedPlugin {
        id: PluginId::parse(id).expect("id"),
        version: PluginVersion::parse("1.0.0").expect("version"),
        directory: std::env::temp_dir().join(id),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::from([(
            "editor".into(),
            Arc::new(json!({"data":{"text":"initial"}})),
        )]),
        localizations: BTreeMap::new(),
        resources: BTreeMap::new(),
    })
}

/// 两个插件同时存活，验证重载不会改变另一插件的实例与 wire。
fn fixture(
    declarative: bool,
) -> (
    Arc<PluginRuntimeService>,
    Arc<TestProcess>,
    Arc<PreparedPlugin>,
    Arc<PreparedPlugin>,
) {
    let one = prepared("acme.notes", declarative);
    let two = prepared("acme.other", declarative);
    let process = Arc::new(TestProcess::new());
    let runtime = PluginRuntimeService::new(
        vec![
            PluginRuntimeDescriptor::from_prepared(one.clone()),
            PluginRuntimeDescriptor::from_prepared(two.clone()),
        ],
        process.clone(),
        Arc::new(TestHost),
        ApplicationTaskExecutor::new(tokio::runtime::Handle::current()),
    )
    .expect("runtime");
    (runtime, process, one, two)
}

/// 同一公开实例始终复用业务身份，进程 wire 必须由服务在重载时重新分配。
fn identity(prepared: &PreparedPlugin) -> PluginViewIdentity {
    PluginViewIdentity {
        plugin: prepared.id.clone(),
        view: "editor".into(),
        instance: "one".into(),
        generation: 1,
    }
}

/// 单一绝对截止时间覆盖排队、重启和视图打开，不逐步累加预算。
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}

/// 显式开发重载等待空打开 ACK，保存状态与其他插件保持有效。
#[tokio::test]
async fn reload_reopens_only_target_with_saved_state_and_acknowledges_empty_reply() {
    let (runtime, process, one, two) = fixture(false);
    let state = PluginViewSavedState {
        state_version: 1,
        state: json!({"draft":"saved"}),
    };
    for plugin in [&one, &two] {
        runtime
            .open(
                identity(plugin),
                json!({"scope":"tab"}),
                Some(state.clone()),
                deadline(),
                CancellationToken::new(),
            )
            .await
            .expect("open");
    }
    let old = process.calls.lock().expect("calls").clone();
    let mut changed = one.as_ref().clone();
    changed.native_views.insert(
        "editor".into(),
        Arc::new(json!({"data":{"text":"updated"}})),
    );
    let mut events = runtime.subscribe();
    assert_eq!(
        runtime
            .dev_reload(Arc::new(changed), deadline(), CancellationToken::new())
            .await
            .expect("reload"),
        2
    );
    let calls = process.calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].0, one.id);
    assert_ne!(calls[2].3["instance"], old[0].3["instance"]);
    assert_eq!(calls[2].3["state"], state.state);
    assert_eq!(
        runtime.cached_state(&identity(&two)).expect("other"),
        Some(state)
    );
    assert_eq!(
        runtime
            .prepared_plugin(&one.id)
            .expect("resource")
            .native_views["editor"]["data"]["text"],
        "updated"
    );
    let mut acknowledged = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, PluginRuntimeEvent::ViewReopened { identity: ref id, process_generation: 2 } if id == &identity(&one))
        {
            acknowledged = true;
        }
    }
    assert!(acknowledged);
}

/// 无效声明在撤销实例或停止进程前失败，当前可用资源完整保留。
#[tokio::test]
async fn manifest_change_is_rejected_before_restart_or_resource_replacement() {
    let (runtime, process, original, _) = fixture(false);
    let mut changed = original.as_ref().clone();
    Arc::make_mut(&mut changed.package).manifest.publisher = "other".into();
    assert!(
        runtime
            .dev_reload(Arc::new(changed), deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(process.restarts.load(Ordering::SeqCst), 0);
    assert!(Arc::ptr_eq(
        &runtime.prepared_plugin(&original.id).expect("original"),
        &original
    ));
}

/// 重启窗口中的关闭先删除失联绑定，新进程就绪后绝不重新创建已关闭视图。
#[tokio::test]
async fn close_during_restart_does_not_resurrect_the_instance() {
    let (runtime, process, original, _) = fixture(false);
    let target = identity(&original);
    runtime
        .open(
            target.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    process.pause_restart.store(true, Ordering::SeqCst);
    let reload = {
        let runtime = runtime.clone();
        let original = original.clone();
        tokio::spawn(async move {
            runtime
                .dev_reload(original, deadline(), CancellationToken::new())
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), process.restart_entered.notified())
        .await
        .expect("restart entered");
    runtime
        .close(&target, deadline())
        .await
        .expect("close disconnected");
    process.restart_release.notify_one();
    reload.await.expect("task").expect("reload");
    assert_eq!(process.calls.lock().expect("calls").len(), 1);
    assert!(runtime.cached_state(&target).is_err());
}

/// 已开始关闭但尚未收到回执的实例不属于重开目标，即使进程在此时换代。
#[tokio::test]
async fn reload_does_not_reopen_a_view_whose_close_is_already_waiting() {
    let (runtime, process, original, _) = fixture(false);
    let target = identity(&original);
    runtime
        .open(
            target.clone(),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    process.pause_close.store(true, Ordering::SeqCst);
    let close = {
        let runtime = runtime.clone();
        let target = target.clone();
        tokio::spawn(async move { runtime.close(&target, deadline()).await })
    };
    tokio::time::timeout(Duration::from_secs(2), process.close_entered.notified())
        .await
        .expect("close entered");
    let reload = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .dev_reload(original, deadline(), CancellationToken::new())
                .await
        })
    };
    process.close_release.notify_one();
    close.await.expect("task").expect("close");
    reload.await.expect("task").expect("reload");
    assert_eq!(
        process
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|(_, _, method, _)| method == "view.open")
            .count(),
        1
    );
    assert!(runtime.cached_state(&target).is_err());
}

/// 开始前取消不得触发任何替换或进程重启。
#[tokio::test]
async fn cancelled_reload_preserves_previous_resources() {
    let (runtime, process, original, _) = fixture(false);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        runtime.dev_reload(original, deadline(), cancellation).await,
        Err(AppError::Cancelled { .. })
    ));
    assert_eq!(process.restarts.load(Ordering::SeqCst), 0);
}

/// 纯声明插件也会替换已打开视图的数据，不能为实现热重载而启动空进程。
#[tokio::test]
async fn declarative_reload_updates_data_without_starting_a_process() {
    let (runtime, process, original, _) = fixture(true);
    runtime
        .open(
            identity(&original),
            json!({}),
            None,
            deadline(),
            CancellationToken::new(),
        )
        .await
        .expect("open");
    let mut changed = original.as_ref().clone();
    changed
        .native_views
        .insert("editor".into(), Arc::new(json!({"data":{"text":"new"}})));
    assert_eq!(
        runtime
            .dev_reload(Arc::new(changed), deadline(), CancellationToken::new())
            .await
            .expect("reload"),
        0
    );
    assert_eq!(
        runtime
            .cached_data(&identity(&original))
            .expect("data")
            .expect("exists")["text"],
        "new"
    );
    assert_eq!(process.restarts.load(Ordering::SeqCst), 0);
}
