//! 开发模式的定向资源换代；正式运行不会调用本入口或重新冻结注册器。
//! @author kongweiguang

use super::*;

/// 仅覆盖已注册插件的资源正文；声明与权限继续使用不可变启动目录。
#[derive(Default)]
pub(super) struct DevRuntimeState {
    prepared: Mutex<BTreeMap<PluginId, Arc<PreparedPlugin>>>,
    active: Mutex<Option<PluginId>>,
    pub(super) gate: tokio::sync::Mutex<()>,
}

/// 正常恢复监听必须让开发重载独占重开，避免同一 ready 帧产生两个 view.open。
pub(super) fn reloading(shared: &Shared, plugin: &PluginId) -> bool {
    shared
        .dev
        .active
        .lock()
        .map_or(true, |active| active.as_ref() == Some(plugin))
}

/// 所有资源读取共用此内存入口，未来新开的视图也能取得最后一次确认的开发资源。
pub(super) fn prepared_plugin(
    shared: &Shared,
    plugin: &PluginId,
) -> AppResult<Arc<PreparedPlugin>> {
    let prepared = shared
        .dev
        .prepared
        .lock()
        .map_err(|_| AppError::infrastructure("开发插件资源锁不可用"))?;
    if let Some(prepared) = prepared.get(plugin) {
        return Ok(prepared.clone());
    }
    shared
        .plugins
        .get(plugin)
        .map(|descriptor| descriptor.prepared.clone())
        .ok_or_else(|| AppError::not_found("插件"))
}

/// RAII 只释放协调标记，已提交的进程 owner 仍由监督器持有并等待真实回收。
struct ReloadLease {
    shared: Arc<Shared>,
}

impl Drop for ReloadLease {
    /// 失败和取消均撤销重开独占，下一次显式重载或用户重试能够继续恢复。
    fn drop(&mut self) {
        self.shared
            .dev
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

impl PluginRuntimeService {
    /// 只有显式开发会话调用；先拒绝声明变化，再取消旧代次并等待目标实例逐一打开确认。
    pub async fn dev_reload(
        &self,
        prepared: Arc<PreparedPlugin>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<u64> {
        let original = self
            .shared
            .plugins
            .get(&prepared.id)
            .ok_or_else(|| AppError::not_found("开发插件"))?;
        validate_contract(&original.prepared, &prepared)?;
        let shared = self.shared.clone();
        self.shared
            .executor
            .run_cancellable(cancellation, move |token| async move {
                let _gate = tokio::select! {
                    biased;
                    _ = token.cancelled() => return Err(AppError::cancelled("开发插件重载已取消")),
                    result = tokio::time::timeout_at(deadline.into(), shared.dev.gate.lock()) => {
                        result.map_err(|_| AppError::timeout("开发插件重载"))?
                    }
                };
                *shared
                    .dev
                    .active
                    .lock()
                    .map_err(|_| AppError::infrastructure("开发插件重载锁不可用"))? =
                    Some(prepared.id.clone());
                let _lease = ReloadLease {
                    shared: shared.clone(),
                };
                let result = reload(&shared, prepared.clone(), deadline, token).await;
                if let Err(error) = &result {
                    let _ = shared
                        .events
                        .send(PluginRuntimeEvent::ResourcesReloadFailed {
                            plugin: prepared.id.clone(),
                            message: error.public_message().into(),
                        });
                }
                result
            })
            .await
    }
}

/// 应用层重复守住冻结边界，避免其他调用方绕过桌面端校验替换授权或配置 codec。
fn validate_contract(original: &PreparedPlugin, prepared: &PreparedPlugin) -> AppResult<()> {
    if original.id != prepared.id
        || original.version != prepared.version
        || original.package.manifest != prepared.package.manifest
        || original.package.configuration_schemas != prepared.package.configuration_schemas
        || original.localizations != prepared.localizations
    {
        return Err(AppError::conflict(
            "插件清单、配置或贡献已改变，请重新启动开发会话",
        ));
    }
    Ok(())
}

/// 在同步锁内撤销旧 wire，随后所有进程等待都发生在锁外；关闭可随时移除失联实例。
async fn reload(
    shared: &Arc<Shared>,
    prepared: Arc<PreparedPlugin>,
    deadline: Instant,
    cancellation: CancellationToken,
) -> AppResult<u64> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("开发插件重载已取消"));
    }
    if Instant::now() >= deadline {
        return Err(AppError::timeout("开发插件重载"));
    }
    ensure_listener(shared);
    let reopening = {
        let mut instances = shared
            .instances
            .lock()
            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
        if instances.iter().any(|(identity, instance)| {
            identity.plugin == prepared.id && instance.data_revision == u64::MAX
        }) {
            return Err(AppError::conflict("插件视图修订号已用尽"));
        }
        // open 也按实例锁再资源锁取初值；相同锁序避免死锁，并让资源换代与绑定失效一起提交。
        shared
            .dev
            .prepared
            .lock()
            .map_err(|_| AppError::infrastructure("开发插件资源锁不可用"))?
            .insert(prepared.id.clone(), prepared.clone());
        let mut reopening = Vec::new();
        for (identity, instance) in instances
            .iter_mut()
            .filter(|(identity, _)| identity.plugin == prepared.id)
        {
            instance.wire_id = uuid::Uuid::new_v4().to_string();
            instance.process_generation = None;
            instance.needs_reopen = true;
            instance.reopening = false;
            // 服务数据恢复到新资源的初值；已确认持久状态另传 view.open，本地未提交草稿由渲染器保留。
            instance.data = prepared
                .native_views
                .get(&identity.view)
                .and_then(|view| view.get("data"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            instance.data_revision += 1;
            reopening.push((identity.clone(), instance.wire_id.clone()));
        }
        reopening
    };
    let _ = shared.events.send(PluginRuntimeEvent::ResourcesReloaded {
        plugin: prepared.id.clone(),
        prepared: prepared.clone(),
    });
    // restart 先回收旧进程树再握手新进程；其中断必须保留 supervisor owner，不能丢弃等待 future。
    let generation = if matches!(prepared.package.manifest.runtime, RuntimeKind::None { .. }) {
        0
    } else {
        shared.process.restart(&prepared.id, deadline).await?
    };
    let mut first_error = None;
    for (identity, wire) in reopening {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("开发插件重载已取消"));
        }
        if let Err(error) = reopen(
            shared,
            identity,
            wire,
            generation,
            deadline,
            cancellation.clone(),
        )
        .await
        {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(generation),
    }
}

/// 每个视图重开前重新核对宿主实例仍存活；关闭后不重建、迟到旧请求不能覆盖新数据。
async fn reopen(
    shared: &Arc<Shared>,
    identity: PluginViewIdentity,
    wire: String,
    generation: u64,
    deadline: Instant,
    cancellation: CancellationToken,
) -> AppResult<()> {
    let opening = {
        let mut instances = shared
            .instances
            .lock()
            .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
        let Some(instance) = instances
            .get_mut(&identity)
            .filter(|instance| instance.wire_id == wire)
        else {
            return Ok(());
        };
        instance.process_generation = Some(generation);
        instance.needs_reopen = false;
        (instance.scope.clone(), instance.state.clone())
    };
    let result = if generation == 0 {
        let data = prepared_plugin(shared, &identity.plugin)?
            .native_views
            .get(&identity.view)
            .and_then(|view| view.get("data"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        apply_data_patch(shared, &identity, generation, &wire, &data)
    } else {
        shared
            .process
            .call_observed(
                &identity.plugin,
                generation,
                "view.open",
                open_params(&identity, &wire, opening.0, opening.1),
                deadline,
                cancellation,
                view_response_observer(
                    shared.clone(),
                    identity.clone(),
                    generation,
                    wire.clone(),
                    "view.open",
                ),
            )
            .await
            .map(|_| ())
    };
    match result {
        Ok(()) => {
            // 关闭可能在 RPC 等待期间完成；缺失实例只表示该视图已退出，不应复活。
            if ensure_binding(shared, &identity, generation, &wire).is_ok() {
                let _ = shared.events.send(PluginRuntimeEvent::ViewReopened {
                    identity,
                    process_generation: generation,
                });
            }
            Ok(())
        }
        Err(error) => {
            let mut instances = shared
                .instances
                .lock()
                .map_err(|_| AppError::infrastructure("插件实例锁不可用"))?;
            if let Some(instance) = instances
                .get_mut(&identity)
                .filter(|instance| instance.wire_id == wire)
            {
                // 同代次 view.open 失败不代表插件端未创建实例，保留绑定供关闭或重试回收。
                instance.needs_reopen = true;
            } else {
                // 已关闭实例收到的取消/失效回执没有恢复对象，不阻止其他视图完成重载。
                return Ok(());
            }
            Err(error)
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/plugin_runtime_dev.rs"]
mod tests;
