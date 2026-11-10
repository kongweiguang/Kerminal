//! 原生视图仅依赖应用端口；桌面在此把 UI 异步契约接到唯一运行服务与 Vault。
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use kerminal_application::{
    AppError, AppResult,
    plugin_ports::{
        PluginProcessPort, PluginRuntime, PluginRuntimeEvent, PluginViewIdentity,
        PluginViewSavedState, PluginWebViewEventSubscription, PluginWebViewEventTarget,
    },
    plugin_runtime::PluginRuntimeService,
    plugin_settings::PluginSecretRepository,
};
use kerminal_domain::plugins::{PluginId, PluginRuntimeState};
use kerminal_ui::plugins::external::runtime::{
    AsyncViewRuntime, RuntimeEvent, RuntimeEventFuture, RuntimeFuture, RuntimeReply,
    RuntimeRequest, RuntimeSubscription, WebViewEvents, WebViewEventsFuture, WebViewHostFuture,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// UI 工厂共享同一适配器，实例之间只交换带完整身份的消息。
pub struct DesktopViewRuntime {
    runtime: Arc<PluginRuntimeService>,
    process: Arc<dyn PluginProcessPort>,
    secrets: Arc<dyn PluginSecretRepository>,
    revisions: Arc<Mutex<BTreeMap<PluginViewIdentity, u64>>>,
}

impl DesktopViewRuntime {
    /// 所有 owner 由组合根注入，适配器不创建进程、线程池或存储根。
    pub fn new(
        runtime: Arc<PluginRuntimeService>,
        process: Arc<dyn PluginProcessPort>,
        secrets: Arc<dyn PluginSecretRepository>,
    ) -> Self {
        Self {
            runtime,
            process,
            secrets,
            revisions: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

/// 静态插件以零代次表示无进程；其它状态不能被视为已准备好。
fn generation(
    process: &dyn PluginProcessPort,
    runtime: &dyn PluginRuntime,
    plugin: &PluginId,
) -> AppResult<u64> {
    if matches!(
        runtime.prepared_plugin(plugin)?.package.manifest.runtime,
        kerminal_plugin_protocol::PluginRuntime::None { .. }
    ) {
        return Ok(0);
    }
    match process.state(plugin)? {
        PluginRuntimeState::Ready { generation } => Ok(generation),
        _ => Err(AppError::conflict("插件进程尚未就绪")),
    }
}

impl AsyncViewRuntime for DesktopViewRuntime {
    /// 应用层完成实例与权限核对，桌面只把真实有界接收流适配成 UI 端口。
    fn subscribe_webview_events(
        &self,
        target: PluginWebViewEventTarget,
        params: Value,
        request_cancellation: CancellationToken,
        lifetime: CancellationToken,
    ) -> WebViewEventsFuture {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            runtime
                .subscribe_webview_events(target, params, request_cancellation, lifetime)
                .map(|stream| Box::new(DesktopWebViewEvents(stream)) as Box<dyn WebViewEvents>)
        })
    }

    /// 页面与插件进程共用应用层门禁；计时留在既有 Tokio 适配层，不让 UI 引入运行时。
    fn route_webview(
        &self,
        identity: PluginViewIdentity,
        method: String,
        params: Value,
        cancellation: CancellationToken,
    ) -> WebViewHostFuture {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let timeout_cancellation = cancellation.clone();
            match tokio::time::timeout(
                Duration::from_secs(30),
                runtime.route_webview(&identity, &method, params, cancellation),
            )
            .await
            {
                Ok(reply) => reply,
                Err(_) => {
                    timeout_cancellation.cancel();
                    Err(kerminal_application::plugin_ports::PluginHostError {
                        code: kerminal_plugin_protocol::runtime::error_code::INTERNAL_ERROR,
                        message: "WebView host request timed out".into(),
                    })
                }
            }
        })
    }

    /// 指定实例重试沿用应用层登记身份及同一截止时间，绝不重放此前动作。
    fn retry_reopen(
        &self,
        identity: PluginViewIdentity,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> RuntimeFuture<u64> {
        let runtime = self.runtime.clone();
        Box::pin(async move { runtime.retry_reopen(identity, deadline, cancellation).await })
    }

    /// 新建视图使用开发重载后已经确认的资源，不重新访问插件目录。
    fn prepared_plugin(
        &self,
        plugin: &PluginId,
    ) -> AppResult<Option<Arc<kerminal_application::plugin_ports::PreparedPlugin>>> {
        self.runtime.prepared_plugin(plugin).map(Some)
    }

    /// 显式映射生命周期方法，不能让任意 UI 方法旁路应用层的实例和权限门禁。
    fn request(&self, request: RuntimeRequest) -> RuntimeFuture<RuntimeReply> {
        let runtime = self.runtime.clone();
        let process = self.process.clone();
        Box::pin(async move {
            let closing = request.method == "view.close";
            let expected_generation = request.process_generation;
            if !closing
                && let Some(expected) = expected_generation
                && generation(process.as_ref(), runtime.as_ref(), &request.identity.plugin)?
                    != expected
            {
                return Err(AppError::conflict("插件进程代次已改变"));
            }
            let identity = request.identity;
            let value = match request.method.as_str() {
                "view.open" => {
                    let saved = match (
                        request.params.get("state_version"),
                        request.params.get("state"),
                    ) {
                        (Some(version), Some(state)) => Some(PluginViewSavedState {
                            state_version: version
                                .as_u64()
                                .and_then(|version| version.try_into().ok())
                                .ok_or_else(|| AppError::invalid("插件视图状态版本"))?,
                            state: state.clone(),
                        }),
                        (None, None) => None,
                        _ => return Err(AppError::invalid("插件视图恢复状态不完整")),
                    };
                    runtime
                        .open(
                            identity.clone(),
                            request
                                .params
                                .get("scope")
                                .cloned()
                                .unwrap_or_else(|| json!({})),
                            saved,
                            request.deadline,
                            request.cancellation,
                        )
                        .await?
                }
                "view.action" | "view.fetch" => {
                    runtime
                        .invoke(
                            &identity,
                            &request.method,
                            request.params,
                            request.deadline,
                            request.cancellation,
                        )
                        .await?
                }
                "view.snapshot" => {
                    let saved = runtime
                        .snapshot(&identity, request.deadline, request.cancellation)
                        .await?;
                    json!({ "state_version": saved.state_version, "state": saved.state })
                }
                "view.close" => match runtime.close(&identity, request.deadline).await? {
                    Some(saved) => {
                        json!({ "state_version": saved.state_version, "state": saved.state })
                    }
                    None => json!({}),
                },
                _ => return Err(AppError::invalid("原生视图请求方法无效")),
            };
            Ok(RuntimeReply {
                // 关闭只确认实例已回收；进程崩溃后没有可查询的就绪代次。
                process_generation: if closing {
                    expected_generation.unwrap_or(0)
                } else {
                    generation(process.as_ref(), runtime.as_ref(), &identity.plugin)?
                },
                value,
            })
        })
    }

    /// 显隐不会销毁进程或重建视图；所有参数都绑定原实例。
    fn notify(&self, request: RuntimeRequest) -> RuntimeFuture<()> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            if request.method != "view.visibility" {
                return Err(AppError::invalid("原生视图通知方法无效"));
            }
            let visible = request
                .params
                .get("visible")
                .and_then(Value::as_bool)
                .ok_or_else(|| AppError::invalid("视图可见性"))?;
            let focused = request
                .params
                .get("focused")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            runtime
                .visibility(request.identity, visible, focused, request.deadline)
                .await
        })
    }

    /// 先建立监听再打开视图，首个补丁不会因异步激活而丢失。
    fn subscribe(&self, identity: &PluginViewIdentity) -> AppResult<Box<dyn RuntimeSubscription>> {
        Ok(Box::new(DesktopViewSubscription {
            receiver: self.runtime.subscribe(),
            identity: identity.clone(),
            revisions: self.revisions.clone(),
        }))
    }

    /// 同步快照仅访问应用内存，不能在 GPUI 保存工作区时阻塞等待进程。
    fn cached_state(
        &self,
        identity: &PluginViewIdentity,
    ) -> AppResult<Option<PluginViewSavedState>> {
        self.runtime.cached_state(identity)
    }

    /// 订阅落后后读取应用层权威数据；UI 仍由自身状态保留未提交草稿。
    fn cached_data(&self, identity: &PluginViewIdentity) -> AppResult<Option<Value>> {
        let mut revisions = self
            .revisions
            .lock()
            .map_err(|_| AppError::infrastructure("插件视图修订锁不可用"))?;
        let snapshot = self.runtime.cached_data_snapshot(identity)?;
        if let Some(snapshot) = &snapshot {
            revisions
                .entry(identity.clone())
                .and_modify(|revision| *revision = (*revision).max(snapshot.revision))
                .or_insert(snapshot.revision);
        }
        Ok(snapshot.map(|snapshot| snapshot.data))
    }

    /// 原生秘密字段只创建 Vault 引用；明文从不进入插件数据快照或 RPC。
    fn store_secret(
        &self,
        identity: PluginViewIdentity,
        binding: String,
        secret: Zeroizing<String>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> RuntimeFuture<String> {
        let secrets = self.secrets.clone();
        let runtime = self.runtime.clone();
        Box::pin(async move {
            runtime.prepared_plugin(&identity.plugin)?;
            tokio::time::timeout_at(
                deadline.into(),
                secrets.create_setting(&identity.plugin, &binding, secret, cancellation),
            )
            .await
            .map_err(|_| AppError::timeout("保存插件秘密"))?
        })
    }
}

struct DesktopWebViewEvents(PluginWebViewEventSubscription);

impl WebViewEvents for DesktopWebViewEvents {
    /// 借用原流维持事件顺序，不建第二条广播管道或隐式绑定活动标签。
    fn recv(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<Option<Value>>> + Send + '_>>
    {
        Box::pin(self.0.recv())
    }
}

struct DesktopViewSubscription {
    receiver: broadcast::Receiver<PluginRuntimeEvent>,
    identity: PluginViewIdentity,
    revisions: Arc<Mutex<BTreeMap<PluginViewIdentity, u64>>>,
}

impl Drop for DesktopViewSubscription {
    /// 订阅随所属实例结束时释放修订游标，长期反复开关标签不会积累身份条目。
    fn drop(&mut self) {
        if let Ok(mut revisions) = self.revisions.lock() {
            revisions.remove(&self.identity);
        }
    }
}

impl RuntimeSubscription for DesktopViewSubscription {
    /// 单个实例消费自己的事件；取消和发送端关闭分别结束等待，不借用当前选中的 Tab。
    fn recv(&mut self, cancellation: CancellationToken) -> RuntimeEventFuture<'_> {
        Box::pin(async move {
            loop {
                let received = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(AppError::cancelled("插件视图订阅已取消")),
                    received = self.receiver.recv() => received,
                };
                match received {
                    Ok(PluginRuntimeEvent::ViewUpdate {
                        identity,
                        process_generation,
                        revision,
                        patch,
                    }) if identity == self.identity => {
                        let mut revisions = self
                            .revisions
                            .lock()
                            .map_err(|_| AppError::infrastructure("插件视图修订锁不可用"))?;
                        let current = revisions.entry(identity.clone()).or_default();
                        if revision <= *current {
                            continue;
                        }
                        *current = revision;
                        return Ok(Some(RuntimeEvent::ViewUpdate {
                            identity,
                            process_generation,
                            patch,
                        }));
                    }
                    Ok(PluginRuntimeEvent::ViewState { identity, state })
                        if identity == self.identity =>
                    {
                        return Ok(Some(RuntimeEvent::ViewState { identity, state }));
                    }
                    Ok(PluginRuntimeEvent::ProcessState { plugin, state })
                        if plugin == self.identity.plugin =>
                    {
                        return Ok(Some(RuntimeEvent::ProcessState { plugin, state }));
                    }
                    Ok(PluginRuntimeEvent::ResourcesReloaded { plugin, prepared })
                        if plugin == self.identity.plugin =>
                    {
                        return Ok(Some(RuntimeEvent::ResourcesReloaded { plugin, prepared }));
                    }
                    Ok(PluginRuntimeEvent::ViewReopened {
                        identity,
                        process_generation,
                    }) if identity == self.identity => {
                        return Ok(Some(RuntimeEvent::ViewReopened {
                            identity,
                            process_generation,
                        }));
                    }
                    Ok(PluginRuntimeEvent::ViewReopenFailed {
                        identity,
                        process_generation,
                        message,
                    }) if identity == self.identity => {
                        return Ok(Some(RuntimeEvent::ViewReopenFailed {
                            identity,
                            process_generation,
                            message,
                        }));
                    }
                    Ok(PluginRuntimeEvent::ResourcesReloadFailed { plugin, message })
                        if plugin == self.identity.plugin =>
                    {
                        return Ok(Some(RuntimeEvent::ResourcesReloadFailed {
                            plugin,
                            message,
                        }));
                    }
                    Ok(PluginRuntimeEvent::Progress {
                        plugin,
                        generation,
                        params,
                    }) if plugin == self.identity.plugin => {
                        return Ok(Some(RuntimeEvent::Progress {
                            identity: self.identity.clone(),
                            process_generation: generation,
                            token: params
                                .get("token")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .into(),
                            message: params
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            percent: params.get("percent").and_then(Value::as_f64),
                        }));
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        return Ok(Some(RuntimeEvent::ResyncRequired { skipped }));
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(None),
                }
            }
        })
    }
}
