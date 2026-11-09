//! 外部插件实例、权限和宿主能力的统一应用用例。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_domain::plugins::{PluginId, PluginRuntimeState};
use kerminal_plugin_protocol::{
    MAX_STATE_BYTES, MAX_VIEW_BYTES, MAX_VIEW_PATCH_BYTES, MAX_VIEW_STATE_BYTES, Permission,
    PluginRuntime as RuntimeKind, ViewActionResult, ViewFetchResult, ViewOpenResult, error_code,
    host_method_permission,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[path = "plugin_runtime_dev.rs"]
mod dev;

use crate::{
    AppError, AppResult,
    executor::ApplicationTaskExecutor,
    plugin_ports::{
        PluginHostApi, PluginHostError, PluginHostRouter, PluginProcessPort,
        PluginResponseObserver, PluginRuntime, PluginRuntimeEvent, PluginViewDataSnapshot,
        PluginViewIdentity, PluginViewSavedState, PreparedPlugin,
    },
};

/// 冻结的清单及授权必须来自同一个已验证插件版本。
#[derive(Clone)]
pub struct PluginRuntimeDescriptor {
    pub prepared: Arc<PreparedPlugin>,
    pub permissions: BTreeSet<Permission>,
}

impl PluginRuntimeDescriptor {
    /// 从冻结清单推导授权，避免组合根单独维护不一致的权限集合。
    pub fn from_prepared(prepared: Arc<PreparedPlugin>) -> Self {
        let manifest = &prepared.package.manifest;
        let mut permissions = manifest
            .permissions
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if matches!(manifest.runtime, RuntimeKind::Sidecar { .. }) {
            permissions.insert(Permission::FullAccess);
        }
        Self {
            prepared,
            permissions,
        }
    }
}

struct Instance {
    wire_id: String,
    process_generation: Option<u64>,
    scope: Value,
    state: Option<PluginViewSavedState>,
    data: Value,
    data_revision: u64,
    needs_reopen: bool,
    reopening: bool,
}

struct Shared {
    plugins: BTreeMap<PluginId, PluginRuntimeDescriptor>,
    process: Arc<dyn PluginProcessPort>,
    host: Arc<dyn PluginHostApi>,
    executor: ApplicationTaskExecutor,
    instances: Mutex<BTreeMap<PluginViewIdentity, Instance>>,
    events: broadcast::Sender<PluginRuntimeEvent>,
    listening: AtomicBool,
    dev: dev::DevRuntimeState,
}

/// 视图状态只在宿主保存，进程重启不能覆盖界面未提交的草稿。
#[derive(Clone)]
pub struct PluginRuntimeService {
    shared: Arc<Shared>,
}

impl PluginRuntimeService {
    /// 组合根只注入已验证启动快照；权限门禁以弱引用注册，避免服务循环持有。
    pub fn new(
        plugins: Vec<PluginRuntimeDescriptor>,
        process: Arc<dyn PluginProcessPort>,
        host: Arc<dyn PluginHostApi>,
        executor: ApplicationTaskExecutor,
    ) -> AppResult<Arc<Self>> {
        let mut entries = BTreeMap::new();
        for plugin in plugins {
            let manifest = &plugin.prepared.package.manifest;
            let mut declared = manifest
                .permissions
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if matches!(manifest.runtime, RuntimeKind::Sidecar { .. }) {
                declared.insert(Permission::FullAccess);
            }
            if manifest.id != plugin.prepared.id.as_str()
                || manifest.version != plugin.prepared.version.to_string()
                || plugin.permissions != declared
            {
                return Err(AppError::invalid("插件运行权限与已验证清单不一致"));
            }
            if entries.insert(plugin.prepared.id.clone(), plugin).is_some() {
                return Err(AppError::conflict("插件运行配置重复"));
            }
        }
        let (events, _) = broadcast::channel(256);
        let service = Arc::new(Self {
            shared: Arc::new(Shared {
                plugins: entries,
                process: Arc::clone(&process),
                host,
                executor,
                instances: Mutex::new(BTreeMap::new()),
                events,
                listening: AtomicBool::new(false),
                dev: dev::DevRuntimeState::default(),
            }),
        });
        let router: Arc<dyn PluginHostRouter> = service.clone();
        process.register_host_router(Arc::downgrade(&router))?;
        Ok(service)
    }

    /// WebView 与进程请求复用同一权限路由；实例身份由宿主覆盖而非信任页面。
    pub async fn route_webview(
        &self,
        identity: &PluginViewIdentity,
        method: &str,
        mut params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        let (generation, wire) =
            binding(&self.shared, identity).map_err(|_| host_error(error_code::STALE, "stale"))?;
        let object = params
            .as_object_mut()
            .ok_or_else(|| host_error(error_code::INVALID_PARAMS, "invalid params"))?;
        object.insert("instance".into(), wire.into());
        let service = self.clone();
        let plugin = identity.plugin.clone();
        let method = method.to_owned();
        self.shared
            .executor
            .run_cancellable(cancellation, move |token| async move {
                Ok(service
                    .route(&plugin, generation, &method, params, token)
                    .await)
            })
            .await
            .map_err(|error| match error {
                AppError::Cancelled { .. } => host_error(error_code::CANCELLED, "cancelled"),
                _ => host_error(error_code::STALE, "stale"),
            })?
    }

    /// 当前就绪代次来自监督器；纯声明插件约定零代次，不创建空进程。
    pub fn process_generation(&self, plugin: &PluginId) -> AppResult<Option<u64>> {
        if !self.shared.plugins.contains_key(plugin) {
            return Err(AppError::not_found("插件"));
        }
        if declarative(&self.shared, plugin) {
            return Ok(Some(0));
        }
        Ok(match self.shared.process.state(plugin)? {
            PluginRuntimeState::Ready { generation } => Some(generation),
            _ => None,
        })
    }

    /// 主动启动与打开视图共享监督器，纯声明插件不触发进程。
    pub async fn activate_plugin(&self, plugin: PluginId, deadline: Instant) -> AppResult<u64> {
        self.activate_or_retry(plugin, deadline, false).await
    }

    /// 用户明确重试才恢复失败状态，不自动重放任何业务请求。
    pub async fn retry_plugin(&self, plugin: PluginId, deadline: Instant) -> AppResult<u64> {
        let generation = self
            .activate_or_retry(plugin.clone(), deadline, true)
            .await?;
        // 进程可能早已 Ready，但某个实例的上次恢复失败仍需要用户显式重试。
        reopen_instances(&self.shared, &plugin, generation);
        Ok(generation)
    }

    /// 显式重试只重开指定实例，并等待真实 view.open 回执，不重放任何业务动作。
    pub async fn retry_reopen(
        &self,
        identity: PluginViewIdentity,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<u64> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件视图恢复已取消"));
        }
        let mut events = self.subscribe();
        let generation = self
            .activate_or_retry(identity.plugin.clone(), deadline, true)
            .await?;
        {
            let instances = self
                .shared
                .instances
                .lock()
                .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
            let instance = instances.get(&identity).ok_or_else(stale)?;
            if !instance.needs_reopen
                && !instance.reopening
                && instance.process_generation == Some(generation)
            {
                return Ok(generation);
            }
        }
        reopen_matching(&self.shared, &identity.plugin, generation, Some(&identity));
        loop {
            let event = tokio::select! {
                _ = cancellation.cancelled() => return Err(AppError::cancelled("插件视图恢复已取消")),
                result = tokio::time::timeout_at(deadline.into(), events.recv()) => {
                    result.map_err(|_| AppError::timeout("插件视图恢复"))?
                        .map_err(|_| AppError::conflict("插件视图恢复事件丢失"))?
                }
            };
            match event {
                PluginRuntimeEvent::ViewReopened {
                    identity: current,
                    process_generation,
                } if current == identity && process_generation == generation => {
                    return Ok(generation);
                }
                PluginRuntimeEvent::ViewReopenFailed {
                    identity: current,
                    process_generation,
                    ..
                } if current == identity && process_generation == generation => {
                    return Err(AppError::conflict("插件视图恢复失败"));
                }
                PluginRuntimeEvent::ViewClosed {
                    identity: current, ..
                } if current == identity => return Err(stale()),
                _ => {}
            }
        }
    }

    /// 启动与重试共用绝对截止时间和共享执行器，不让 UI 拥有进程。
    async fn activate_or_retry(
        &self,
        plugin: PluginId,
        deadline: Instant,
        retry: bool,
    ) -> AppResult<u64> {
        if !self.shared.plugins.contains_key(&plugin) {
            return Err(AppError::not_found("插件"));
        }
        if dev::reloading(&self.shared, &plugin) {
            return Err(AppError::conflict("插件正在开发重载"));
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("插件启动"));
        }
        if declarative(&self.shared, &plugin) {
            return Ok(0);
        }
        let shared = Arc::clone(&self.shared);
        self.shared
            .executor
            .run(move |_| async move {
                ensure_listener(&shared);
                if retry {
                    shared.process.retry(&plugin, deadline).await
                } else {
                    shared.process.activate(&plugin, deadline).await
                }
            })
            .await
    }

    /// 普通读取可只取数据；订阅恢复须使用带修订号的快照过滤已包含的事件。
    pub fn cached_data(&self, identity: &PluginViewIdentity) -> AppResult<Option<Value>> {
        Ok(self
            .cached_data_snapshot(identity)?
            .map(|snapshot| snapshot.data))
    }

    /// 数据与事件水位在同一锁内取得，恢复时可安全丢弃不大于该水位的缓冲补丁。
    pub fn cached_data_snapshot(
        &self,
        identity: &PluginViewIdentity,
    ) -> AppResult<Option<PluginViewDataSnapshot>> {
        let instances = self
            .shared
            .instances
            .lock()
            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
        Ok(instances
            .get(identity)
            .map(|instance| PluginViewDataSnapshot {
                revision: instance.data_revision,
                data: instance.data.clone(),
            }))
    }

    /// 非视图操作同样通过共享执行器和指定插件代次，不自动重放写操作。
    pub async fn call_plugin(
        &self,
        plugin: PluginId,
        method: String,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value> {
        if !matches!(method.as_str(), "command.run" | "host.test" | "host.open") {
            return Err(AppError::invalid("插件调用方法"));
        }
        if !self.shared.plugins.contains_key(&plugin) {
            return Err(AppError::not_found("插件"));
        }
        if dev::reloading(&self.shared, &plugin) {
            return Err(AppError::conflict("插件正在开发重载"));
        }
        if declarative(&self.shared, &plugin) {
            return Err(AppError::unsupported("声明式插件没有运行进程"));
        }
        let shared = Arc::clone(&self.shared);
        self.shared
            .executor
            .run_cancellable(cancellation, move |token| async move {
                ensure_listener(&shared);
                let generation = shared.process.activate(&plugin, deadline).await?;
                shared
                    .process
                    .call(&plugin, generation, &method, params, deadline, token)
                    .await
            })
            .await
    }

    /// 明确通知进程视图的可见性，隐藏不会销毁实例或其草稿。
    pub async fn visibility(
        &self,
        identity: PluginViewIdentity,
        visible: bool,
        focused: bool,
        deadline: Instant,
    ) -> AppResult<()> {
        let shared = Arc::clone(&self.shared);
        self.shared
            .executor
            .run(move |_| async move {
                let (generation, wire) = binding(&shared, &identity)?;
                if declarative(&shared, &identity.plugin) {
                    return Ok(());
                }
                shared
                    .process
                    .notify(
                        &identity.plugin,
                        generation,
                        "view.visibility",
                        json!({"instance":wire,"visible":visible,"focused":focused}),
                        deadline,
                    )
                    .await
            })
            .await
    }
}

#[async_trait]
impl PluginRuntime for PluginRuntimeService {
    /// 静态资源在启动快照中冻结，读取不会触发进程或文件 I/O。
    fn prepared_plugin(&self, id: &PluginId) -> AppResult<Arc<PreparedPlugin>> {
        dev::prepared_plugin(&self.shared, id)
    }

    /// 先登记独立实例，再发送带随机 wire 身份的打开请求，迟到回执无法命中重开实例。
    async fn open(
        &self,
        identity: PluginViewIdentity,
        scope: Value,
        state: Option<PluginViewSavedState>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value> {
        let descriptor = self
            .shared
            .plugins
            .get(&identity.plugin)
            .ok_or_else(|| AppError::not_found("插件"))?;
        if !descriptor
            .prepared
            .package
            .manifest
            .views
            .contains_key(&identity.view)
        {
            return Err(AppError::not_found("插件视图"));
        }
        if let Some(saved) = &state {
            validate_state(saved)?;
        }
        let shared = Arc::clone(&self.shared);
        self.shared
            .executor
            .run_cancellable(cancellation, move |token| async move {
                // 与开发重载共享门禁，确保快照之后不会登记仍绑定旧进程的新实例。
                let _gate = tokio::select! {
                    _ = token.cancelled() => return Err(AppError::cancelled("插件视图打开已取消")),
                    result = tokio::time::timeout_at(deadline.into(), shared.dev.gate.lock()) => {
                        result.map_err(|_| AppError::timeout("插件视图打开"))?
                    }
                };
                ensure_listener(&shared);
                let wire_id = uuid::Uuid::new_v4().to_string();
                {
                    let mut instances = shared
                        .instances
                        .lock()
                        .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
                    if instances.contains_key(&identity) {
                        return Err(AppError::conflict("插件视图实例已存在"));
                    }
                    instances.insert(
                        identity.clone(),
                        Instance {
                            wire_id: wire_id.clone(),
                            process_generation: None,
                            scope: scope.clone(),
                            state: state.clone(),
                            data: initial_view_data(&shared, &identity)?,
                            data_revision: 0,
                            needs_reopen: false,
                            reopening: false,
                        },
                    );
                }
                let result = async {
                    if declarative(&shared, &identity.plugin) {
                        let mut instances = shared
                            .instances
                            .lock()
                            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
                        let instance = instances
                            .get_mut(&identity)
                            .filter(|instance| instance.wire_id == wire_id)
                            .ok_or_else(stale)?;
                        instance.process_generation = Some(0);
                        return Ok(json!({}));
                    }
                    let generation = shared.process.activate(&identity.plugin, deadline).await?;
                    {
                        let mut instances = shared
                            .instances
                            .lock()
                            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
                        let instance = instances
                            .get_mut(&identity)
                            .filter(|instance| instance.wire_id == wire_id)
                            .ok_or_else(stale)?;
                        instance.process_generation = Some(generation);
                    }
                    let observer = view_response_observer(
                        Arc::clone(&shared),
                        identity.clone(),
                        generation,
                        wire_id.clone(),
                        "view.open",
                    );
                    let result = shared
                        .process
                        .call_observed(
                            &identity.plugin,
                            generation,
                            "view.open",
                            open_params(&identity, &wire_id, scope, state),
                            deadline,
                            token,
                            observer,
                        )
                        .await?;
                    ensure_binding(&shared, &identity, generation, &wire_id)?;
                    Ok(result)
                }
                .await;
                if result.is_err()
                    && let Ok(mut instances) = shared.instances.lock()
                    && instances
                        .get(&identity)
                        .is_some_and(|instance| instance.wire_id == wire_id)
                {
                    instances.remove(&identity);
                }
                result
            })
            .await
    }

    /// 动作及分页固定实例身份，调用参数不能自行替换视图或进程目标。
    async fn invoke(
        &self,
        identity: &PluginViewIdentity,
        method: &str,
        mut params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value> {
        if !matches!(method, "view.action" | "view.fetch") {
            return Err(AppError::invalid("插件视图调用方法"));
        }
        let identity = identity.clone();
        let method = method.to_owned();
        let shared = Arc::clone(&self.shared);
        self.shared
            .executor
            .run_cancellable(cancellation, move |token| async move {
                let (generation, wire) = binding(&shared, &identity)?;
                if declarative(&shared, &identity.plugin) {
                    return Err(AppError::unsupported("声明式插件没有运行进程"));
                }
                let object = params
                    .as_object_mut()
                    .ok_or_else(|| AppError::invalid("插件视图调用参数"))?;
                object.insert("instance".into(), wire.clone().into());
                object.insert("view".into(), identity.view.clone().into());
                let observed_method = if method == "view.action" {
                    "view.action"
                } else {
                    "view.fetch"
                };
                let observer = view_response_observer(
                    Arc::clone(&shared),
                    identity.clone(),
                    generation,
                    wire.clone(),
                    observed_method,
                );
                let result = shared
                    .process
                    .call_observed(
                        &identity.plugin,
                        generation,
                        &method,
                        params,
                        deadline,
                        token,
                        observer,
                    )
                    .await?;
                ensure_binding(&shared, &identity, generation, &wire)?;
                Ok(result)
            })
            .await
    }

    /// 同步快照只返回已校验的状态，供无异步能力的视图恢复使用。
    fn cached_state(
        &self,
        identity: &PluginViewIdentity,
    ) -> AppResult<Option<PluginViewSavedState>> {
        let instances = self
            .shared
            .instances
            .lock()
            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
        Ok(instances.get(identity).ok_or_else(stale)?.state.clone())
    }

    /// 回执通过身份与大小校验后才更新缓存和通知订阅者。
    async fn snapshot(
        &self,
        identity: &PluginViewIdentity,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<PluginViewSavedState> {
        let identity = identity.clone();
        let shared = Arc::clone(&self.shared);
        self.shared
            .executor
            .run_cancellable(cancellation, move |token| async move {
                let (generation, wire) = binding(&shared, &identity)?;
                if declarative(&shared, &identity.plugin) {
                    let instances = shared
                        .instances
                        .lock()
                        .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
                    return instances
                        .get(&identity)
                        .and_then(|instance| instance.state.clone())
                        .ok_or_else(|| AppError::not_found("插件视图状态"));
                }
                let value = shared
                    .process
                    .call(
                        &identity.plugin,
                        generation,
                        "view.snapshot",
                        json!({"instance":wire}),
                        deadline,
                        token,
                    )
                    .await?;
                let state =
                    parse_state(&value)?.ok_or_else(|| AppError::invalid("插件快照缺少状态"))?;
                update_state(&shared, &identity, generation, &wire, state.clone())?;
                Ok(state)
            })
            .await
    }

    /// 成功或超时都回收宿主实例；超时仍返回错误，最终状态只使用已确认缓存。
    async fn close(
        &self,
        identity: &PluginViewIdentity,
        deadline: Instant,
    ) -> AppResult<Option<PluginViewSavedState>> {
        let identity = identity.clone();
        let shared = Arc::clone(&self.shared);
        self.shared
            .executor
            .run(move |token| async move {
                // 已失联的实例可在重载等待进程期间直接删除，避免关闭和重启互相等待。
                if let Some(state) = finish_disconnected_view_close(&shared, &identity)? {
                    return Ok(state);
                }
                // 关闭与开发重载不能交叉替换 wire，否则关闭回执可能误判并复活视图。
                let _gate = tokio::select! {
                    _ = token.cancelled() => return Err(AppError::cancelled("插件视图关闭已取消")),
                    result = tokio::time::timeout_at(deadline.into(), shared.dev.gate.lock()) => {
                        result.map_err(|_| AppError::timeout("插件视图关闭"))?
                    }
                };
                // 进程崩溃后没有可发送关闭请求的代次，但仍必须释放宿主实例并保留已确认状态。
                if let Some(state) = finish_disconnected_view_close(&shared, &identity)? {
                    return Ok(state);
                }
                let (generation, wire) = match binding(&shared, &identity) {
                    Ok(binding) => binding,
                    Err(error) => {
                        return finish_disconnected_view_close(&shared, &identity)?
                            .ok_or(error);
                    }
                };
                let state = if declarative(&shared, &identity.plugin) {
                    None
                } else {
                    match shared
                        .process
                        .call(
                            &identity.plugin,
                            generation,
                            "view.close",
                            json!({"instance":wire}),
                            deadline,
                            token,
                        )
                        .await
                    {
                        Ok(value) => parse_state(&value)?,
                        Err(error) => {
                            let timed_out = matches!(error, AppError::Timeout { .. });
                            let process_gone = shared.process.state(&identity.plugin).is_ok_and(|state| {
                                !matches!(state, PluginRuntimeState::Ready { generation: current } if current == generation)
                            });
                            if timed_out && !process_gone {
                                let instances = shared.instances.lock().map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
                                if instances.get(&identity).is_some_and(|instance| instance.wire_id == wire && (instance.needs_reopen || instance.reopening)) {
                                    // 失败 open 后插件端可能已创建实例，关闭未知时保留 owner 供再次回收。
                                    return Err(error);
                                }
                            }
                            if timed_out || process_gone {
                                // 崩溃事件可能先清除绑定，也可能尚未投递；两种顺序都回收同一实例。
                                let saved = match finish_view_close(&shared, &identity, generation, &wire, None) {
                                    Ok(saved) => Some(saved),
                                    Err(_) => finish_disconnected_view_close(&shared, &identity)?,
                                };
                                if let Some(saved) = saved {
                                    return if timed_out { Err(error) } else { Ok(saved) };
                                }
                            }
                            return Err(error);
                        }
                    }
                };
                finish_view_close(&shared, &identity, generation, &wire, state)
            })
            .await
    }

    /// 事件带完整实例身份；消费者落后时可从缓存重新读取状态。
    fn subscribe(&self) -> broadcast::Receiver<PluginRuntimeEvent> {
        self.shared.events.subscribe()
    }
}

#[async_trait]
impl PluginHostRouter for PluginRuntimeService {
    /// 同步入口供 RPC 读取器串行提交视图补丁；不等待异步宿主能力。
    fn ordered_view_update(
        &self,
        plugin: &PluginId,
        generation: u64,
        params: Value,
    ) -> Result<Value, PluginHostError> {
        route_view_update(&self.shared, plugin, generation, params)
    }

    /// 所有来源先按协议方法检查权限，再按进程和实例代次绑定具体宿主能力。
    async fn route(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        if cancellation.is_cancelled() {
            return Err(host_error(error_code::CANCELLED, "cancelled"));
        }
        if method == "view.update" {
            return route_view_update(&self.shared, plugin, generation, params);
        }
        let descriptor = self
            .shared
            .plugins
            .get(plugin)
            .ok_or_else(|| host_error(error_code::STALE, "stale"))?;
        if !(declarative(&self.shared, plugin) && generation == 0) {
            match self.shared.process.state(plugin) {
                Ok(PluginRuntimeState::Ready {
                    generation: current,
                }) if current == generation => {}
                _ => return Err(host_error(error_code::STALE, "stale")),
            }
        }
        let permission = host_method_permission(method)
            .map_err(|_| host_error(error_code::METHOD_NOT_FOUND, "method not found"))?;
        if permission.is_some_and(|permission| {
            !descriptor.permissions.contains(&permission)
                && !descriptor.permissions.contains(&Permission::FullAccess)
        }) {
            return Err(host_error(
                error_code::PERMISSION_DENIED,
                "permission denied",
            ));
        }
        let object = params
            .as_object()
            .ok_or_else(|| host_error(error_code::INVALID_PARAMS, "invalid params"))?;
        let wire = object.get("instance").and_then(Value::as_str);
        let identity = if let Some(wire) = wire {
            let instances = self
                .shared
                .instances
                .lock()
                .map_err(|_| host_error(error_code::STALE, "stale"))?;
            Some(
                instances
                    .iter()
                    .find(|(identity, instance)| {
                        &identity.plugin == plugin
                            && instance.process_generation == Some(generation)
                            && instance.wire_id == wire
                    })
                    .map(|(identity, _)| identity.clone())
                    .ok_or_else(|| host_error(error_code::STALE, "stale"))?,
            )
        } else {
            None
        };
        if matches!(
            method,
            "view.update" | "ui.confirm" | "ui.prompt" | "ui.pickFile" | "terminal.paste"
        ) && identity.is_none()
        {
            return Err(host_error(error_code::INVALID_PARAMS, "instance required"));
        }
        if method == "$/progress" {
            let _ = self.shared.events.send(PluginRuntimeEvent::Progress {
                plugin: plugin.clone(),
                generation,
                params,
            });
            return Ok(json!({}));
        }
        if method == "state.set" {
            let state = object
                .get("state")
                .ok_or_else(|| host_error(error_code::INVALID_PARAMS, "state required"))?;
            if json_size(state).is_none_or(|size| size > MAX_STATE_BYTES) {
                return Err(host_error(error_code::INVALID_PARAMS, "state too large"));
            }
        }
        if method == "shell.openExternal" {
            let url = object
                .get("url")
                .and_then(Value::as_str)
                .and_then(|value| url::Url::parse(value).ok());
            if !url.is_some_and(|url| {
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
            }) {
                return Err(host_error(error_code::INVALID_PARAMS, "https URL required"));
            }
        }
        if cancellation.is_cancelled() {
            return Err(host_error(error_code::CANCELLED, "cancelled"));
        }
        self.shared
            .host
            .invoke(
                plugin,
                generation,
                identity.as_ref(),
                method,
                params,
                cancellation,
            )
            .await
    }
}

/// 相同的内存路由用于插件同步帧和 WebView，身份校验与补丁提交均不得退回其他实例。
fn route_view_update(
    shared: &Shared,
    plugin: &PluginId,
    generation: u64,
    params: Value,
) -> Result<Value, PluginHostError> {
    if !shared.plugins.contains_key(plugin) {
        return Err(host_error(error_code::STALE, "stale"));
    }
    if !(declarative(shared, plugin) && generation == 0)
        && !matches!(shared.process.state(plugin), Ok(PluginRuntimeState::Ready { generation: current }) if current == generation)
    {
        return Err(host_error(error_code::STALE, "stale"));
    }
    let object = params
        .as_object()
        .ok_or_else(|| host_error(error_code::INVALID_PARAMS, "invalid params"))?;
    let wire = object
        .get("instance")
        .and_then(Value::as_str)
        .ok_or_else(|| host_error(error_code::INVALID_PARAMS, "instance required"))?;
    let patch = object
        .get("patch")
        .ok_or_else(|| host_error(error_code::INVALID_PARAMS, "patch required"))?;
    if json_size(patch).is_none_or(|size| size > MAX_VIEW_PATCH_BYTES) {
        return Err(host_error(error_code::INVALID_PARAMS, "patch too large"));
    }
    let identity = {
        let instances = shared
            .instances
            .lock()
            .map_err(|_| host_error(error_code::STALE, "stale"))?;
        instances
            .iter()
            .find(|(identity, instance)| {
                &identity.plugin == plugin
                    && instance.process_generation == Some(generation)
                    && instance.wire_id == wire
            })
            .map(|(identity, _)| identity.clone())
            .ok_or_else(|| host_error(error_code::STALE, "stale"))?
    };
    apply_data_patch(shared, &identity, generation, wire, patch).map_err(|error| match error {
        AppError::Conflict { .. } => host_error(error_code::STALE, "stale"),
        _ => host_error(error_code::INVALID_PARAMS, "invalid view data"),
    })?;
    Ok(json!({}))
}

/// 回执观察器只做同步校验与内存提交，保证同一连接后续帧不会越过视图结果。
fn view_response_observer(
    shared: Arc<Shared>,
    identity: PluginViewIdentity,
    generation: u64,
    wire: String,
    method: &'static str,
) -> PluginResponseObserver {
    Box::new(move |value| {
        validate_view_result(method, value)?;
        let field = match method {
            "view.open" => Some("data"),
            "view.action" => Some("patch"),
            _ => None,
        };
        if let Some(patch) = field
            .and_then(|field| value.get(field))
            .filter(|patch| !patch.is_null())
        {
            apply_data_patch(&shared, &identity, generation, &wire, patch)
        } else {
            ensure_binding(&shared, &identity, generation, &wire)
        }
    })
}

/// 纯声明插件没有进程，但仍可显示视图并经相同权限门禁使用宿主能力。
fn declarative(shared: &Shared, plugin: &PluginId) -> bool {
    shared.plugins.get(plugin).is_some_and(|descriptor| {
        matches!(
            descriptor.prepared.package.manifest.runtime,
            RuntimeKind::None { .. }
        )
    })
}

/// 在共享应用执行器登记状态监听；服务本身不会建立新的 Tokio runtime。
fn ensure_listener(shared: &Arc<Shared>) {
    if shared.listening.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut events = shared.process.subscribe();
    let owned = Arc::clone(shared);
    let executor = shared.executor.clone();
    tokio::spawn(async move {
        let listening = Arc::clone(&owned);
        let _ = executor.run(move |cancellation| async move {
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    event = events.recv() => match event {
                        Ok(event) if event.method == "$/process-state" => {
                            let state = event.params.get("state").and_then(Value::as_str).unwrap_or("");
                            match state {
                                "crashed" => mark_crashed(&owned, &event.plugin, event.generation),
                                "ready" => reopen_instances(&owned, &event.plugin, event.generation),
                                _ => {}
                            }
                            if let Ok(state) = owned.process.state(&event.plugin) {
                                let _ = owned.events.send(PluginRuntimeEvent::ProcessState { plugin: event.plugin, state });
                            }
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => reconcile(&owned),
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            Ok(())
        }).await;
        listening.listening.store(false, Ordering::Release);
    });
}

/// 崩溃仅撤销匹配代次的绑定，最后状态和宿主草稿仍保留。
fn mark_crashed(shared: &Shared, plugin: &PluginId, generation: u64) {
    if let Ok(mut instances) = shared.instances.lock() {
        for (identity, instance) in instances.iter_mut() {
            if &identity.plugin == plugin && instance.process_generation == Some(generation) {
                instance.process_generation = None;
                instance.needs_reopen = true;
                instance.reopening = false;
            }
        }
    }
}

/// 监听落后时读取监督器现状，发现代次变化后重开仍存活的实例。
fn reconcile(shared: &Arc<Shared>) {
    for plugin in shared.plugins.keys() {
        if let Ok(state) = shared.process.state(plugin) {
            match state {
                PluginRuntimeState::Ready { generation } => {
                    reopen_instances(shared, plugin, generation)
                }
                PluginRuntimeState::Crashed { generation } => {
                    mark_crashed(shared, plugin, generation)
                }
                _ => {}
            }
            let _ = shared.events.send(PluginRuntimeEvent::ProcessState {
                plugin: plugin.clone(),
                state,
            });
        }
    }
}

/// 每次进程恢复分配新 wire 身份；保留的状态用于打开，草稿继续由界面持有。
fn reopen_instances(shared: &Arc<Shared>, plugin: &PluginId, generation: u64) {
    reopen_matching(shared, plugin, generation, None);
}

/// 显式重试可以限定实例，自动恢复则处理所有需要重开的实例。
fn reopen_matching(
    shared: &Arc<Shared>,
    plugin: &PluginId,
    generation: u64,
    target: Option<&PluginViewIdentity>,
) {
    if dev::reloading(shared, plugin) {
        return;
    }
    if !matches!(shared.process.state(plugin), Ok(PluginRuntimeState::Ready { generation: current }) if current == generation)
    {
        return;
    }
    let reopening = match shared.instances.lock() {
        Ok(mut instances) => instances
            .iter_mut()
            .filter_map(|(identity, instance)| {
                if &identity.plugin != plugin || target.is_some_and(|target| target != identity) {
                    return None;
                }
                if instance
                    .process_generation
                    .is_some_and(|current| current != generation)
                {
                    instance.needs_reopen = true;
                }
                if !instance.needs_reopen || instance.reopening {
                    return None;
                }
                // 同代次的失败 open 可能已经创建插件实例；保留旧 wire 直到 close 确认。
                let previous = (instance.process_generation == Some(generation))
                    .then(|| instance.wire_id.clone());
                let wire = uuid::Uuid::new_v4().to_string();
                if previous.is_none() {
                    instance.wire_id = wire.clone();
                    instance.process_generation = Some(generation);
                }
                instance.needs_reopen = false;
                instance.reopening = true;
                Some((
                    identity.clone(),
                    wire,
                    previous,
                    instance.scope.clone(),
                    instance.state.clone(),
                ))
            })
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    for (identity, wire, previous, scope, mut state) in reopening {
        let shared = Arc::clone(shared);
        let executor = shared.executor.clone();
        tokio::spawn(async move {
            let _ = executor
                .run(move |token| async move {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    // 与 close 和开发重载共用门禁；等待期间实例可能已关闭，发送前须重新核对。
                    let result = async {
                        let _gate = tokio::select! {
                            _ = token.cancelled() => return Err(AppError::cancelled("插件视图恢复已取消")),
                            result = tokio::time::timeout_at(deadline.into(), shared.dev.gate.lock()) => {
                                result.map_err(|_| AppError::timeout("插件视图恢复"))?
                            }
                        };
                        if let Some(previous) = &previous {
                            ensure_binding(&shared, &identity, generation, previous)?;
                            let closed = shared.process.call(
                                &identity.plugin, generation, "view.close",
                                json!({"instance": previous}), deadline, token.child_token(),
                            ).await?;
                            let saved = parse_state(&closed)?;
                            let mut instances = shared.instances.lock()
                                .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
                            let instance = instances.get_mut(&identity)
                                .filter(|instance| instance.wire_id == *previous && instance.process_generation == Some(generation))
                                .ok_or_else(stale)?;
                            if let Some(saved) = saved {
                                instance.state = Some(saved.clone());
                                state = Some(saved);
                            }
                            instance.wire_id = wire.clone();
                        } else {
                            ensure_binding(&shared, &identity, generation, &wire)?;
                        }
                        let observer = view_response_observer(
                            Arc::clone(&shared), identity.clone(), generation, wire.clone(), "view.open",
                        );
                        let response = shared.process.call_observed(
                            &identity.plugin, generation, "view.open",
                            open_params(&identity, &wire, scope, state), deadline, token, observer,
                        ).await?;
                        ensure_binding(&shared, &identity, generation, &wire)?;
                        Ok::<_, AppError>(response)
                    }.await;
                    match result {
                        Ok(_) => {
                            ensure_binding(&shared, &identity, generation, &wire)?;
                            if let Ok(mut instances) = shared.instances.lock()
                                && let Some(instance) = instances.get_mut(&identity).filter(|instance| instance.wire_id == wire)
                            {
                                instance.reopening = false;
                            }
                            let _ = shared.events.send(PluginRuntimeEvent::ViewReopened {
                                identity: identity.clone(),
                                process_generation: generation,
                            });
                        }
                        Err(error) => {
                            if let Ok(mut instances) = shared.instances.lock()
                                && let Some(instance) = instances
                                    .get_mut(&identity)
                                    .filter(|instance| (instance.wire_id == wire || previous.as_ref().is_some_and(|old| &instance.wire_id == old)) && instance.process_generation == Some(generation))
                            {
                                // close 或 open 失败均保留仍可能存在的 wire，后续关闭或重试继续回收。
                                instance.needs_reopen = true;
                                instance.reopening = false;
                                let _ = shared.events.send(PluginRuntimeEvent::ViewReopenFailed {
                                    identity: identity.clone(),
                                    process_generation: generation,
                                    message: error.public_message().into(),
                                });
                            }
                            return Err(error);
                        }
                    }
                    Ok(())
                })
                .await;
        });
    }
}

/// 只在视图确无初值时返回空对象，资源锁错误必须传给调用方。
fn initial_view_data(shared: &Shared, identity: &PluginViewIdentity) -> AppResult<Value> {
    let prepared = dev::prepared_plugin(shared, &identity.plugin)?;
    Ok(prepared
        .native_views
        .get(&identity.view)
        .and_then(|view| view.get("data"))
        .cloned()
        .unwrap_or_else(|| json!({})))
}

/// 先在副本上合并并限制总量，再原子替换，拒绝超限更新不污染权威缓存。
fn apply_data_patch(
    shared: &Shared,
    identity: &PluginViewIdentity,
    generation: u64,
    wire: &str,
    patch: &Value,
) -> AppResult<()> {
    if json_size(patch).is_none_or(|size| size > MAX_VIEW_PATCH_BYTES) {
        return Err(AppError::invalid("插件视图补丁过大"));
    }
    let mut instances = shared
        .instances
        .lock()
        .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
    let instance = instances
        .get_mut(identity)
        .filter(|instance| {
            instance.process_generation == Some(generation) && instance.wire_id == wire
        })
        .ok_or_else(stale)?;
    let mut updated = instance.data.clone();
    merge_data_patch(&mut updated, patch);
    if json_size(&updated).is_none_or(|size| size > MAX_VIEW_BYTES) {
        return Err(AppError::invalid("插件视图数据过大"));
    }
    let revision = instance
        .data_revision
        .checked_add(1)
        .ok_or_else(|| AppError::conflict("插件视图修订号耗尽"))?;
    instance.data = updated;
    instance.data_revision = revision;
    let _ = shared.events.send(PluginRuntimeEvent::ViewUpdate {
        identity: identity.clone(),
        process_generation: generation,
        revision,
        patch: patch.clone(),
    });
    Ok(())
}

/// RFC 7386 的 null 删除对象键，其余非对象值替换整个目标。
fn merge_data_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(patch) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        if let Value::Object(target) = target {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(key);
                } else {
                    merge_data_patch(target.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
        }
    } else {
        *target = patch.clone();
    }
}

/// 应用层 wire 实例与公开实例分离，避免关闭后重用业务 ID 接收旧消息。
fn binding(shared: &Shared, identity: &PluginViewIdentity) -> AppResult<(u64, String)> {
    let instances = shared
        .instances
        .lock()
        .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
    let instance = instances.get(identity).ok_or_else(stale)?;
    Ok((
        instance.process_generation.ok_or_else(stale)?,
        instance.wire_id.clone(),
    ))
}

/// 每次异步回执都重新检查绑定，防止关闭、重开或崩溃后更新新实例。
fn ensure_binding(
    shared: &Shared,
    identity: &PluginViewIdentity,
    generation: u64,
    wire: &str,
) -> AppResult<()> {
    let (current, current_wire) = binding(shared, identity)?;
    if current != generation || current_wire != wire {
        return Err(stale());
    }
    Ok(())
}

/// 崩溃后实例不再绑定任何进程；在同一锁内删除可阻止并发重启重新打开已关闭的视图。
fn finish_disconnected_view_close(
    shared: &Shared,
    identity: &PluginViewIdentity,
) -> AppResult<Option<Option<PluginViewSavedState>>> {
    let saved = {
        let mut instances = shared
            .instances
            .lock()
            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
        let instance = instances.get(identity).ok_or_else(stale)?;
        if instance.process_generation.is_some() {
            return Ok(None);
        }
        let saved = instance.state.clone();
        instances.remove(identity);
        saved
    };
    let _ = shared.events.send(PluginRuntimeEvent::ViewClosed {
        identity: identity.clone(),
        state: saved.clone(),
    });
    Ok(Some(saved))
}
/// 关闭回执或超时后移除原实例并发布已确认状态，迟到消息无法命中重开实例。
fn finish_view_close(
    shared: &Shared,
    identity: &PluginViewIdentity,
    generation: u64,
    wire: &str,
    state: Option<PluginViewSavedState>,
) -> AppResult<Option<PluginViewSavedState>> {
    let final_state = {
        let mut instances = shared
            .instances
            .lock()
            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
        let instance = instances
            .get(identity)
            .filter(|instance| {
                instance.process_generation == Some(generation) && instance.wire_id == wire
            })
            .ok_or_else(stale)?;
        let saved = state.or_else(|| instance.state.clone());
        instances.remove(identity);
        saved
    };
    let _ = shared.events.send(PluginRuntimeEvent::ViewClosed {
        identity: identity.clone(),
        state: final_state.clone(),
    });
    Ok(final_state)
}

/// 状态对象只在确认原始实例仍存活后替换，避免旧快照覆盖新草稿。
fn update_state(
    shared: &Shared,
    identity: &PluginViewIdentity,
    generation: u64,
    wire: &str,
    state: PluginViewSavedState,
) -> AppResult<()> {
    {
        let mut instances = shared
            .instances
            .lock()
            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
        let instance = instances
            .get_mut(identity)
            .filter(|instance| {
                instance.process_generation == Some(generation) && instance.wire_id == wire
            })
            .ok_or_else(stale)?;
        instance.state = Some(state.clone());
    }
    let _ = shared.events.send(PluginRuntimeEvent::ViewState {
        identity: identity.clone(),
        state,
    });
    Ok(())
}

/// 插件回执必须符合单一协议类型，补丁上限不能只依赖传输帧上限。
fn validate_view_result(method: &str, value: &Value) -> AppResult<()> {
    let patch = match method {
        "view.open" => {
            serde_json::from_value::<ViewOpenResult>(value.clone())
                .map_err(|_| AppError::invalid("插件视图打开回复"))?
                .data
        }
        "view.action" => {
            serde_json::from_value::<ViewActionResult>(value.clone())
                .map_err(|_| AppError::invalid("插件视图动作回复"))?
                .patch
        }
        "view.fetch" => {
            serde_json::from_value::<ViewFetchResult>(value.clone())
                .map_err(|_| AppError::invalid("插件视图分页回复"))?;
            None
        }
        _ => return Err(AppError::invalid("插件视图调用方法")),
    };
    if patch
        .as_ref()
        .is_some_and(|patch| json_size(patch).is_none_or(|size| size > MAX_VIEW_PATCH_BYTES))
    {
        return Err(AppError::invalid("插件视图补丁过大"));
    }
    Ok(())
}

/// 可选状态按协议成对提供，空回复不能伪造一个新状态。
fn parse_state(value: &Value) -> AppResult<Option<PluginViewSavedState>> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::invalid("插件状态回复"))?;
    match (object.get("state_version"), object.get("state")) {
        (None, None) => Ok(None),
        (Some(version), Some(state)) => {
            let version = version
                .as_u64()
                .and_then(|version| u32::try_from(version).ok())
                .ok_or_else(|| AppError::invalid("插件状态版本"))?;
            let saved = PluginViewSavedState {
                state_version: version,
                state: state.clone(),
            };
            validate_state(&saved)?;
            Ok(Some(saved))
        }
        _ => Err(AppError::invalid("插件状态字段不完整")),
    }
}

/// 状态大小按协议限制，不能依赖界面层在恢复时才发现超限。
fn validate_state(state: &PluginViewSavedState) -> AppResult<()> {
    if json_size(&state.state).is_none_or(|size| size > MAX_VIEW_STATE_BYTES) {
        return Err(AppError::invalid("插件视图状态过大"));
    }
    Ok(())
}

/// 状态仅发送插件定义的 JSON，版本由宿主保存并在恢复前校验。
fn open_params(
    identity: &PluginViewIdentity,
    wire: &str,
    scope: Value,
    state: Option<PluginViewSavedState>,
) -> Value {
    let mut params = json!({"view":identity.view,"instance":wire,"scope":scope});
    if let Some(state) = state {
        params["state"] = state.state;
    }
    params
}

/// 序列化失败按不可信数据拒绝，不能绕过大小约束。
fn json_size(value: &Value) -> Option<usize> {
    serde_json::to_vec(value).ok().map(|bytes| bytes.len())
}

/// 只返回固定协议文案，避免把宿主路径和凭据泄漏给插件。
fn host_error(code: i32, message: &'static str) -> PluginHostError {
    PluginHostError {
        code,
        message: message.into(),
    }
}

/// 实例代次变化是显式冲突，不能退回当前活动视图。
fn stale() -> AppError {
    AppError::conflict("插件视图实例已失效")
}
