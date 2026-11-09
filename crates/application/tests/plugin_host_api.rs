//! 插件宿主应用适配的身份、脱敏、取消与事件回归。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, connections,
    plugin_host_api::{
        PluginEventSubscriptions, PluginHostApiService, PluginHostSummary, PluginHostsPort,
        PluginUiPort,
    },
    plugin_ports::{
        PluginHostApi, PluginHostError, PluginHostRouter, PluginProcessMessage, PluginProcessPort,
        PluginRuntime, PluginViewIdentity, PreparedPlugin,
    },
    plugin_runtime::{PluginRuntimeDescriptor, PluginRuntimeService},
    plugin_settings::{
        PluginSecretRepository, PluginSettingsRepository, PluginSettingsService,
        PluginStateRepository, StoredPluginSettings,
    },
};
use kerminal_domain::{
    plugins::{PluginId, PluginRuntimeState, PluginVersion},
    preferences::ConfigRevision,
};
use kerminal_plugin_protocol::{
    ConfigurationSchema, ManifestPolicy, Permission, ValidatedPackage, error_code, parse_manifest,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[derive(Default)]
struct MemoryStore {
    settings: Mutex<BTreeMap<PluginId, Value>>,
    states: Mutex<BTreeMap<PluginId, Value>>,
    secrets: Mutex<BTreeMap<(PluginId, String), String>>,
}

#[async_trait]
impl PluginSettingsRepository for MemoryStore {
    /// 配置快照按插件独立取出，便于验证 schema 与身份配对。
    async fn load(
        &self,
        plugin: &PluginId,
        _: CancellationToken,
    ) -> AppResult<StoredPluginSettings> {
        Ok(StoredPluginSettings {
            values: self.settings.lock().unwrap().get(plugin).cloned(),
            revision: None,
        })
    }
    /// 本用例只覆盖运行时读取，拒绝意外写入而非伪造提交。
    async fn save(
        &self,
        _: &PluginId,
        _: Value,
        _: Option<ConfigRevision>,
        _: CancellationToken,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported("测试不支持设置写入"))
    }
    /// 本用例不触发删除设置，防止遗漏的调用被误当作成功。
    async fn delete(
        &self,
        _: &PluginId,
        _: Option<ConfigRevision>,
        _: CancellationToken,
    ) -> AppResult<Option<ConfigRevision>> {
        Err(AppError::unsupported("测试不支持设置删除"))
    }
}

#[async_trait]
impl PluginStateRepository for MemoryStore {
    /// 不存在的状态保持缺失，避免测试依赖伪造默认值。
    async fn get(&self, plugin: &PluginId, _: CancellationToken) -> AppResult<Option<Value>> {
        Ok(self.states.lock().unwrap().get(plugin).cloned())
    }
    /// 状态只按已验证插件身份写入，不接收外部路径。
    async fn set(&self, plugin: &PluginId, state: Value, _: CancellationToken) -> AppResult<()> {
        self.states.lock().unwrap().insert(plugin.clone(), state);
        Ok(())
    }
    /// 清理仅删除测试目标的命名空间。
    async fn delete_data(&self, plugin: &PluginId, _: CancellationToken) -> AppResult<()> {
        self.states.lock().unwrap().remove(plugin);
        Ok(())
    }
}

#[async_trait]
impl PluginSecretRepository for MemoryStore {
    /// 设置保存不在这些运行时用例中，不能静默模拟 Vault 写入。
    async fn create_setting(
        &self,
        _: &PluginId,
        _: &str,
        _: Zeroizing<String>,
        _: CancellationToken,
    ) -> AppResult<String> {
        Err(AppError::unsupported("测试不支持设置写入"))
    }
    /// 测试引用必须匹配插件和字段，确保脱敏检查使用真实服务的解密分支。
    async fn read_setting(
        &self,
        plugin: &PluginId,
        path: &str,
        reference: &str,
        _: CancellationToken,
    ) -> AppResult<Zeroizing<String>> {
        if reference != format!("ref:{}:{path}", plugin.as_str()) {
            return Err(AppError::invalid("引用不属于插件"));
        }
        Ok(Zeroizing::new("fixture-private-secret".into()))
    }
    /// 槽位按插件和键一起查找，模拟 Vault 的命名空间约束。
    async fn get(
        &self,
        plugin: &PluginId,
        slot: &str,
        _: CancellationToken,
    ) -> AppResult<Option<Zeroizing<String>>> {
        Ok(self
            .secrets
            .lock()
            .unwrap()
            .get(&(plugin.clone(), slot.into()))
            .cloned()
            .map(Zeroizing::new))
    }
    /// 独立保存同名槽，避免测试仅验证不同槽的隔离。
    async fn set(
        &self,
        plugin: &PluginId,
        slot: &str,
        value: Zeroizing<String>,
        _: CancellationToken,
    ) -> AppResult<()> {
        self.secrets
            .lock()
            .unwrap()
            .insert((plugin.clone(), slot.into()), value.to_string());
        Ok(())
    }
    /// 幂等删除只作用于指定插件。
    async fn delete(&self, plugin: &PluginId, slot: &str, _: CancellationToken) -> AppResult<bool> {
        Ok(self
            .secrets
            .lock()
            .unwrap()
            .remove(&(plugin.clone(), slot.into()))
            .is_some())
    }
    /// 数据清理在测试中保留其他插件的槽。
    async fn delete_all(&self, plugin: &PluginId, _: CancellationToken) -> AppResult<()> {
        self.secrets
            .lock()
            .unwrap()
            .retain(|(owner, _), _| owner != plugin);
        Ok(())
    }
}

struct Hosts;
#[async_trait]
impl PluginHostsPort for Hosts {
    /// 模拟端口只能提供类型化白名单，不暴露整个主机配置。
    async fn list(&self, _: CancellationToken) -> AppResult<Vec<PluginHostSummary>> {
        Ok(vec![PluginHostSummary {
            id: "host-1".into(),
            name: "Example".into(),
            kind: "ssh".into(),
            group_id: None,
        }])
    }
}

#[derive(Default)]
struct Ui {
    calls: Mutex<Vec<(String, Option<PluginViewIdentity>, Value)>>,
}
#[async_trait]
impl PluginUiPort for Ui {
    /// 记录可信实例与实际参数；错误注入验证动态信息不会回传插件。
    async fn invoke(
        &self,
        _: &PluginId,
        instance: Option<&PluginViewIdentity>,
        method: &str,
        params: Value,
        _: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), instance.cloned(), params));
        if method == "ui.prompt" {
            return Err(PluginHostError {
                code: error_code::INTERNAL_ERROR,
                message: "private-path-and-secret".into(),
            });
        }
        Ok(json!({"accepted":true}))
    }
}

struct Process {
    states: Mutex<BTreeMap<PluginId, PluginRuntimeState>>,
    sent: Mutex<Vec<(PluginId, u64, String, Value)>>,
    changes: broadcast::Sender<PluginProcessMessage>,
}
impl Process {
    /// 模拟监督器的代次状态，与事件发送记录共用同一实例。
    fn new(ids: &[PluginId]) -> Self {
        let (changes, _) = broadcast::channel(8);
        Self {
            states: Mutex::new(
                ids.iter()
                    .cloned()
                    .map(|id| (id, PluginRuntimeState::Ready { generation: 1 }))
                    .collect(),
            ),
            sent: Mutex::new(Vec::new()),
            changes,
        }
    }
}
#[async_trait]
impl PluginProcessPort for Process {
    /// 本测试只覆盖事件通知，不建立反向路由。
    fn register_host_router(&self, _: Weak<dyn PluginHostRouter>) -> AppResult<()> {
        Ok(())
    }
    /// 按插件读取实际模拟状态，以覆盖进程重启和插件隔离。
    fn state(&self, plugin: &PluginId) -> AppResult<PluginRuntimeState> {
        self.states
            .lock()
            .unwrap()
            .get(plugin)
            .cloned()
            .ok_or_else(|| AppError::not_found("进程不存在"))
    }
    /// 激活只返回模拟进程的就绪代次，使真实运行路由可固定视图身份。
    async fn activate(&self, plugin: &PluginId, _: Instant) -> AppResult<u64> {
        match self.state(plugin)? {
            PluginRuntimeState::Ready { generation } => Ok(generation),
            _ => Err(AppError::conflict("进程未就绪")),
        }
    }
    /// 仅模拟打开视图，其他调用显式失败，避免把意外动作误作成功。
    async fn call(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        _: Value,
        _: Instant,
        _: CancellationToken,
    ) -> AppResult<Value> {
        if self.state(plugin)? != (PluginRuntimeState::Ready { generation }) {
            return Err(AppError::conflict("代次已过期"));
        }
        if method != "view.open" {
            return Err(AppError::unsupported("测试只允许打开视图"));
        }
        Ok(json!({"data":{}}))
    }
    /// 验证通知固定在指定代次，并记录可交付的事件。
    async fn notify(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        _: Instant,
    ) -> AppResult<()> {
        if self.state(plugin)? != (PluginRuntimeState::Ready { generation }) {
            return Err(AppError::conflict("代次已过期"));
        }
        self.sent
            .lock()
            .unwrap()
            .push((plugin.clone(), generation, method.into(), params));
        Ok(())
    }
    /// 模拟进程事件流保留真实端口的订阅语义。
    fn subscribe(&self) -> broadcast::Receiver<PluginProcessMessage> {
        self.changes.subscribe()
    }
    /// 模拟进程不持有操作系统资源。
    async fn shutdown_until(&self, _: Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 测试包冻结独立 schema，并通过与生产相同的清单解析器校验身份。
fn prepared(id: &str) -> Arc<PreparedPlugin> {
    let manifest = json!({
        "schema": 1, "id": id, "name": {"en":"Example"}, "description": {"en":"Example"},
        "version": "1.0.0", "api": "^1.0", "publisher": "acme",
        "runtime": {"kind":"script", "main":"main.js"},
        "contributes": {"settings": {"schema":"settings.schema.json"}}
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&manifest).unwrap(),
        ManifestPolicy::ThirdParty,
    )
    .unwrap();
    let schema = ConfigurationSchema::parse(
        br#"{
        "type":"object", "required":["enabled","token"],
        "properties":{"enabled":{"type":"boolean"},"token":{"type":"string","x-secret":true}}
    }"#,
    )
    .unwrap();
    Arc::new(PreparedPlugin {
        id: PluginId::parse(id).unwrap(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        directory: PathBuf::from("fixture"),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::from([("settings.schema.json".into(), schema)]),
        }),
        native_views: BTreeMap::new(),
        localizations: BTreeMap::new(),
        resources: BTreeMap::new(),
    })
}

struct Fixture {
    api: PluginHostApiService,
    events: Arc<PluginEventSubscriptions>,
    store: Arc<MemoryStore>,
    process: Arc<Process>,
    ui: Arc<Ui>,
    first: PluginId,
    second: PluginId,
}
/// 同一个共享服务模拟两个已安装插件，捕获跨插件泄漏和代次混用。
fn fixture() -> Fixture {
    let first = PluginId::parse("acme.notes").unwrap();
    let second = PluginId::parse("acme.other").unwrap();
    let store = Arc::new(MemoryStore::default());
    let settings = PluginSettingsService::new(store.clone(), store.clone(), store.clone());
    let process = Arc::new(Process::new(&[first.clone(), second.clone()]));
    let events = Arc::new(PluginEventSubscriptions::new(process.clone()));
    let ui = Arc::new(Ui::default());
    let api = PluginHostApiService::new(
        vec![prepared(first.as_str()), prepared(second.as_str())],
        settings,
        Arc::new(Hosts),
        ui.clone(),
        events.clone(),
    )
    .unwrap();
    Fixture {
        api,
        events,
        store,
        process,
        ui,
        first,
        second,
    }
}
/// 所有普通请求使用新令牌，只有取消测试才共享特殊令牌。
async fn invoke(
    api: &PluginHostApiService,
    plugin: &PluginId,
    instance: Option<&PluginViewIdentity>,
    method: &str,
    params: Value,
) -> Result<Value, PluginHostError> {
    api.invoke(
        plugin,
        1,
        instance,
        method,
        params,
        CancellationToken::new(),
    )
    .await
}

#[tokio::test]
/// 设置返回通过冻结 schema 验证的脱敏快照，既不泄漏明文也不泄漏 Vault 引用。
async fn settings_read_uses_plugin_schema_and_redacts_secrets() {
    let f = fixture();
    f.store.settings.lock().unwrap().insert(
        f.first.clone(),
        json!({"enabled":true, "token":"ref:acme.notes:/token"}),
    );
    let result = invoke(&f.api, &f.first, None, "settings.get", json!({}))
        .await
        .unwrap();
    assert_eq!(result, json!({"enabled":true,"token":null}));
    assert!(!result.to_string().contains("fixture-private-secret"));
    assert!(!result.to_string().contains("ref:"));
    let empty = invoke(&f.api, &f.second, None, "settings.get", json!({}))
        .await
        .unwrap();
    assert_eq!(empty, json!({}));
}

#[tokio::test]
/// 状态和同名秘密槽均按插件隔离，超限和已取消写入不能改变仓储。
async fn state_and_secrets_are_isolated_and_bounded() {
    let f = fixture();
    invoke(
        &f.api,
        &f.first,
        None,
        "state.set",
        json!({"state":{"x":1}}),
    )
    .await
    .unwrap();
    assert_eq!(
        invoke(&f.api, &f.second, None, "state.get", json!({}))
            .await
            .unwrap(),
        json!({"state":null})
    );
    let too_large = invoke(
        &f.api,
        &f.first,
        None,
        "state.set",
        json!({"state":"界".repeat(350_000)}),
    )
    .await
    .unwrap_err();
    assert_eq!(too_large.code, error_code::INVALID_PARAMS);
    assert_eq!(
        invoke(&f.api, &f.first, None, "state.get", json!({}))
            .await
            .unwrap(),
        json!({"state":{"x":1}})
    );
    for (plugin, secret) in [(&f.first, "secret-one"), (&f.second, "secret-two")] {
        invoke(
            &f.api,
            plugin,
            None,
            "secrets.set",
            json!({"slot":"same", "value":secret}),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        invoke(
            &f.api,
            &f.first,
            None,
            "secrets.get",
            json!({"slot":"same"})
        )
        .await
        .unwrap(),
        json!({"value":"secret-one"})
    );
    invoke(
        &f.api,
        &f.first,
        None,
        "secrets.delete",
        json!({"slot":"same"}),
    )
    .await
    .unwrap();
    assert_eq!(
        invoke(
            &f.api,
            &f.second,
            None,
            "secrets.get",
            json!({"slot":"same"})
        )
        .await
        .unwrap(),
        json!({"value":"secret-two"})
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = f
        .api
        .invoke(
            &f.first,
            1,
            None,
            "state.set",
            json!({"state":"late"}),
            cancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::CANCELLED);
    assert_eq!(
        invoke(&f.api, &f.first, None, "state.get", json!({}))
            .await
            .unwrap(),
        json!({"state":{"x":1}})
    );
}

#[tokio::test]
/// 窗口操作固定调用实例，非法 HTTPS 与跨插件身份不能抵达 UI。
async fn ui_delegation_fails_closed_for_wrong_identity_and_url() {
    let f = fixture();
    let identity = PluginViewIdentity {
        plugin: f.first.clone(),
        view: "editor".into(),
        instance: "tab".into(),
        generation: 3,
    };
    assert_eq!(
        invoke(
            &f.api,
            &f.first,
            None,
            "terminal.paste",
            json!({"text":"x"})
        )
        .await
        .unwrap_err()
        .code,
        error_code::INVALID_PARAMS
    );
    assert_eq!(
        invoke(&f.api, &f.second, Some(&identity), "ui.confirm", json!({}))
            .await
            .unwrap_err()
            .code,
        error_code::STALE
    );
    for url in [
        "http://example.com",
        "https://secret:password@example.com",
        "file:///secret",
    ] {
        assert_eq!(
            invoke(
                &f.api,
                &f.first,
                None,
                "shell.openExternal",
                json!({"url":url})
            )
            .await
            .unwrap_err()
            .code,
            error_code::INVALID_PARAMS
        );
    }
    invoke(
        &f.api,
        &f.first,
        Some(&identity),
        "terminal.paste",
        json!({"instance":"spoofed", "text":"x"}),
    )
    .await
    .unwrap();
    invoke(
        &f.api,
        &f.first,
        None,
        "shell.openExternal",
        json!({"url":"https://example.com"}),
    )
    .await
    .unwrap();
    let error = invoke(&f.api, &f.first, Some(&identity), "ui.prompt", json!({}))
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (error_code::INTERNAL_ERROR, "internal error")
    );
    let calls = f.ui.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].1.as_ref(), Some(&identity));
    assert_eq!(calls[0].2, json!({"text":"x"}));
}

#[tokio::test]
/// 事件只投递订阅名称和原进程代次，重启或令牌取消后不会自动继承。
async fn events_are_bound_to_generation_and_subscription_lifetime() {
    let f = fixture();
    let token = CancellationToken::new();
    f.api
        .invoke(
            &f.first,
            1,
            None,
            "events.subscribe",
            json!({"names":["host.connected"]}),
            token.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.events
            .publish(
                "tab.changed",
                json!({"tab":"one"}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.events
            .publish(
                "host.connected",
                json!({"host":"one"}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap(),
        1
    );
    f.process
        .states
        .lock()
        .unwrap()
        .insert(f.first.clone(), PluginRuntimeState::Ready { generation: 2 });
    assert_eq!(
        f.events
            .publish(
                "host.connected",
                json!({}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap(),
        0
    );
    let token2 = CancellationToken::new();
    f.api
        .invoke(
            &f.first,
            2,
            None,
            "events.subscribe",
            json!({"names":["host.connected"]}),
            token2.clone(),
        )
        .await
        .unwrap();
    token2.cancel();
    assert_eq!(
        f.events
            .publish(
                "host.connected",
                json!({}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap(),
        0
    );
    let sent = f.process.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].1, sent[0].2.as_str()), (1, "event"));
    assert_eq!(
        sent[0].3,
        json!({"name":"host.connected", "payload":{"host":"one"}})
    );
}

#[tokio::test]
/// 视图订阅固定路由器提供的原代次，旧请求不能覆盖或取消新进程的订阅。
async fn view_subscription_uses_trusted_generation_after_process_restart() {
    let f = fixture();
    let identity = PluginViewIdentity {
        plugin: f.first.clone(),
        view: "editor".into(),
        instance: "tab".into(),
        generation: 3,
    };
    f.api
        .invoke(
            &f.first,
            1,
            Some(&identity),
            "events.subscribe",
            json!({"names":["host.connected"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.events
            .publish(
                "host.connected",
                json!({}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap(),
        1
    );
    f.process
        .states
        .lock()
        .unwrap()
        .insert(f.first.clone(), PluginRuntimeState::Ready { generation: 2 });
    f.api
        .invoke(
            &f.first,
            2,
            Some(&identity),
            "events.subscribe",
            json!({"names":["host.connected"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for names in [json!(["tab.changed"]), json!([])] {
        let error = f
            .api
            .invoke(
                &f.first,
                1,
                Some(&identity),
                "events.subscribe",
                json!({"names":names}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::STALE);
    }
    assert_eq!(
        f.events
            .publish(
                "host.connected",
                json!({}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap(),
        1
    );
    let sent = f.process.sent.lock().unwrap();
    assert_eq!(
        sent.iter().map(|event| event.1).collect::<Vec<_>>(),
        vec![1, 2]
    );
}
#[tokio::test]
/// WebView 请求经过真实权限路由后，订阅仍绑定打开视图时的进程代次。
async fn runtime_routes_webview_subscription_with_verified_generation() {
    let f = fixture();
    let mut prepared_plugin = (*prepared(f.first.as_str())).clone();
    let manifest = &mut Arc::make_mut(&mut prepared_plugin.package).manifest;
    manifest.permissions.push(Permission::EventsSubscribe);
    manifest.views.insert(
        "editor".into(),
        kerminal_plugin_protocol::ViewDefinition::Webview {
            entry: "views/editor.html".into(),
            csp: None,
            extensions: BTreeMap::new(),
        },
    );
    let prepared_plugin = Arc::new(prepared_plugin);
    let settings = PluginSettingsService::new(f.store.clone(), f.store.clone(), f.store.clone());
    let api = Arc::new(
        PluginHostApiService::new(
            vec![prepared_plugin.clone()],
            settings,
            Arc::new(Hosts),
            f.ui.clone(),
            f.events.clone(),
        )
        .unwrap(),
    );
    let runtime = PluginRuntimeService::new(
        vec![PluginRuntimeDescriptor {
            prepared: prepared_plugin,
            permissions: BTreeSet::from([Permission::EventsSubscribe]),
        }],
        f.process.clone(),
        api,
        ApplicationTaskExecutor::new(tokio::runtime::Handle::current()),
    )
    .unwrap();
    let identity = PluginViewIdentity {
        plugin: f.first.clone(),
        view: "editor".into(),
        instance: "tab".into(),
        generation: 3,
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    runtime
        .open(
            identity.clone(),
            json!({}),
            None,
            deadline,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    runtime
        .route_webview(
            &identity,
            "events.subscribe",
            json!({"names":["host.connected"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.events
            .publish("host.connected", json!({}), deadline)
            .await
            .unwrap(),
        1
    );
    let sent = f.process.sent.lock().unwrap();
    assert_eq!((sent[0].1, sent[0].2.as_str()), (1, "event"));
}
#[tokio::test]
/// 主机返回值限定为目录摘要，未知插件和未知方法不能绕过主路由。
async fn host_projection_and_invalid_calls_are_explicit() {
    let f = fixture();
    let hosts = invoke(&f.api, &f.first, None, "hosts.list", json!({}))
        .await
        .unwrap();
    assert_eq!(
        hosts,
        json!({"hosts":[{"id":"host-1","name":"Example","kind":"ssh","group_id":null}]})
    );
    assert_eq!(
        invoke(
            &f.api,
            &PluginId::parse("acme.unknown").unwrap(),
            None,
            "state.get",
            json!({})
        )
        .await
        .unwrap_err()
        .code,
        error_code::STALE
    );
    assert_eq!(
        invoke(&f.api, &f.first, None, "unknown", json!({}))
            .await
            .unwrap_err()
            .code,
        error_code::METHOD_NOT_FOUND
    );
    assert_eq!(
        invoke(&f.api, &f.first, None, "secrets.get", json!({"slot":""}))
            .await
            .unwrap_err()
            .code,
        error_code::INVALID_PARAMS
    );
}

struct CredentialHosts;

#[async_trait]
impl connections::management::HostManagementRepository for CredentialHosts {
    /// 本测试严格只读，不允许误把目录查询升级为写入。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Err(AppError::unsupported("只读仓储"))
    }
    /// 真实主机对象包含凭据引用和其他敏感字段，用于检验白名单投影。
    async fn list_hosts(&self) -> AppResult<Vec<connections::management::ManagedHost>> {
        use connections::management::{HostAuthenticationMode, HostRouteSettings, ManagedHost};
        use kerminal_domain::connections::{
            CredentialRef, HostEndpoint, RemoteProtocol, SshAuthentication,
        };
        Ok(vec![ManagedHost {
            id: kerminal_domain::identity::HostId::parse("remote").unwrap(),
            group_id: None,
            name: "remote".into(),
            note: Some("sensitive-note".into()),
            protocol: RemoteProtocol::Ssh,
            endpoint: HostEndpoint {
                host: "sensitive-host".into(),
                port: 22,
                username: "sensitive-user".into(),
            },
            authentication: Some(SshAuthentication::Password {
                secret: CredentialRef::new("sensitive-reference").unwrap(),
            }),
            authentication_mode: HostAuthenticationMode::Password,
            rdp_password: None,
            route: HostRouteSettings::default(),
            serial: None,
            tags: vec![],
            sort_order: 0,
            connect_timeout_ms: 1000,
            keepalive_interval_ms: None,
            terminal_options: Default::default(),
            sftp_options: Default::default(),
            rdp_display: Default::default(),
            created_at: "1".into(),
            updated_at: "1".into(),
            revision: ConfigRevision::from_sha256("r1".into()),
        }])
    }
    /// 本地环境引用、参数和目录不得跟随主机列表对外输出。
    async fn list_local_hosts(&self) -> AppResult<Vec<connections::management::ManagedLocalHost>> {
        Ok(vec![connections::management::ManagedLocalHost {
            id: kerminal_domain::identity::HostId::parse("local").unwrap(),
            group_id: None,
            name: "local".into(),
            shell: "sensitive-shell".into(),
            args: vec!["sensitive-arg".into()],
            cwd: Some("sensitive-cwd".into()),
            environment_ref: Some(
                kerminal_domain::connections::CredentialRef::new("sensitive-env-reference")
                    .unwrap(),
            ),
            sort_order: 0,
            created_at: "1".into(),
            updated_at: "1".into(),
            revision: ConfigRevision::from_sha256("r2".into()),
            migration_origin: None,
        }])
    }
    /// 插件配置是任意数据，不能因为字段未被宿主认识就直接序列化。
    async fn list_plugin_hosts(&self) -> AppResult<Vec<connections::host_types::PluginHost>> {
        Ok(vec![connections::host_types::PluginHost {
            id: kerminal_domain::identity::HostId::parse("custom").unwrap(),
            provider_id: "example".into(),
            type_id: "example.custom".into(),
            name: "custom".into(),
            group_id: None,
            sort_order: 0,
            configuration_version: 1,
            configuration: Some(toml::from_str("private = 'sensitive-plugin-value'").unwrap()),
            runtime_available: true,
            revision: ConfigRevision::from_sha256("r3".into()),
        }])
    }
    /// 其他仓储方法不应在只读列表路径上调用。
    async fn load_host(
        &self,
        _: &kerminal_domain::identity::HostId,
    ) -> AppResult<Option<connections::management::ManagedHost>> {
        Err(AppError::unsupported("只读仓储"))
    }
    /// 测试仓储拒绝保存，避免误认一个模拟写入成功。
    async fn save_host(
        &self,
        _: kerminal_domain::identity::HostId,
        _: connections::management::HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<connections::management::ManagedHost> {
        Err(AppError::unsupported("只读仓储"))
    }
    /// 测试仓储拒绝删除，保持列表夹具不可变。
    async fn delete_host(
        &self,
        _: &kerminal_domain::identity::HostId,
        _: &ConfigRevision,
    ) -> AppResult<connections::management::ManagedHost> {
        Err(AppError::unsupported("只读仓储"))
    }
    /// 主机投影不需要读取分组内容。
    async fn load_groups(&self) -> AppResult<connections::management::HostGroupSnapshot> {
        Err(AppError::unsupported("只读仓储"))
    }
    /// 分组写入不属于此测试。
    async fn save_group(
        &self,
        _: connections::management::HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<connections::management::HostGroupSnapshot> {
        Err(AppError::unsupported("只读仓储"))
    }
    /// 分组删除不属于此测试。
    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<connections::management::HostDocumentRevision>,
    ) -> AppResult<connections::management::HostGroupDeletionReceipt> {
        Err(AppError::unsupported("只读仓储"))
    }
}

#[tokio::test]
/// 真实管理服务将远程、本地和插件主机全部投影为白名单，任何配置正文都不能进入插件响应。
async fn actual_host_management_projection_excludes_credentials_and_configuration() {
    let f = fixture();
    let hosts = Arc::new(connections::management::HostManagementService::new(
        Arc::new(CredentialHosts),
    ));
    let settings = PluginSettingsService::new(f.store.clone(), f.store.clone(), f.store.clone());
    let api = PluginHostApiService::new(
        vec![prepared(f.first.as_str())],
        settings,
        hosts,
        f.ui.clone(),
        f.events.clone(),
    )
    .unwrap();
    let result = invoke(&api, &f.first, None, "hosts.list", json!({}))
        .await
        .unwrap();
    assert_eq!(result["hosts"].as_array().unwrap().len(), 3);
    assert!(!result.to_string().contains("sensitive-"));
    assert_eq!(result["hosts"][0]["kind"], "ssh");
    assert_eq!(result["hosts"][1]["kind"], "local");
    assert_eq!(result["hosts"][2]["kind"], "example.custom");
}
