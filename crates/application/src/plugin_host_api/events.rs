//! 进程代次绑定的插件事件订阅；事件来源由组合根显式发布。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};

use kerminal_domain::plugins::{PluginId, PluginRuntimeState};
use kerminal_plugin_protocol::error_code;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    plugin_ports::{
        PluginHostError, PluginProcessPort, PluginViewIdentity, PluginWebViewEventSubscription,
        PluginWebViewEventTarget,
    },
};

use super::host_error;

struct Subscription {
    generation: u64,
    names: BTreeSet<String>,
    cancellation: CancellationToken,
}

type PageKey = (PluginViewIdentity, u64);
struct PageSubscription {
    target: PluginWebViewEventTarget,
    names: BTreeSet<String>,
    cancellation: CancellationToken,
    sender: tokio::sync::mpsc::Sender<Value>,
}

/// 订阅不持有进程或窗口实例；进程结束时令牌失效，旧订阅不能复用于重启后的代次。
pub struct PluginEventSubscriptions {
    process: Arc<dyn PluginProcessPort>,
    subscriptions: Mutex<BTreeMap<PluginId, Arc<Subscription>>>,
    pages: Arc<Mutex<BTreeMap<PageKey, Arc<PageSubscription>>>>,
}

impl PluginEventSubscriptions {
    /// 与运行服务共享同一个监督器，通知始终按订阅时的代次发送。
    pub fn new(process: Arc<dyn PluginProcessPort>) -> Self {
        Self {
            process,
            subscriptions: Mutex::new(BTreeMap::new()),
            pages: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// 页面和进程槽位分离；空名称集合保留序列墓碑，阻止更早的在途订阅重新生效。
    pub fn subscribe_webview(
        &self,
        target: PluginWebViewEventTarget,
        names: Vec<String>,
        lifetime: CancellationToken,
    ) -> Result<PluginWebViewEventSubscription, PluginHostError> {
        if lifetime.is_cancelled() {
            return Err(host_error(error_code::CANCELLED));
        }
        if target.page_epoch == 0
            || target.page_epoch > 9_007_199_254_740_991
            || target.sequence == 0
            || names.len() > 128
            || names.iter().any(|name| {
                name.is_empty() || name.len() > 128 || name.chars().any(char::is_control)
            })
        {
            return Err(host_error(error_code::INVALID_PARAMS));
        }
        if target.process_generation != 0
            && self.process.state(&target.identity.plugin).ok()
                != Some(PluginRuntimeState::Ready {
                    generation: target.process_generation,
                })
        {
            return Err(host_error(error_code::STALE));
        }
        let key = (target.identity.clone(), target.page_epoch);
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let entry = Arc::new(PageSubscription {
            target,
            names: names.into_iter().collect(),
            cancellation: lifetime.child_token(),
            sender,
        });
        {
            let mut pages = self
                .pages
                .lock()
                .map_err(|_| host_error(error_code::INTERNAL_ERROR))?;
            if entry.cancellation.is_cancelled() {
                return Err(host_error(error_code::CANCELLED));
            }
            if pages
                .get(&key)
                .is_some_and(|current| current.target.sequence >= entry.target.sequence)
            {
                return Err(host_error(error_code::STALE));
            }
            if let Some(previous) = pages.insert(key.clone(), entry.clone()) {
                previous.cancellation.cancel();
            }
        }
        let pages = Arc::downgrade(&self.pages);
        let cancellation = entry.cancellation.clone();
        let plugin = entry.target.identity.plugin.clone();
        let generation = entry.target.process_generation;
        let stream = PluginWebViewEventSubscription::new(
            receiver,
            cancellation.clone(),
            Box::new(move || {
                if let Some(pages) = pages.upgrade()
                    && let Ok(mut pages) = pages.lock()
                    && pages
                        .get(&key)
                        .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    pages.remove(&key);
                }
            }),
        );
        // 登记期间可能发生关闭或重启；失败时让接收端 Drop 撤销刚写入的槽位。
        if cancellation.is_cancelled() {
            return Err(host_error(error_code::CANCELLED));
        }
        if generation != 0
            && self.process.state(&plugin).ok() != Some(PluginRuntimeState::Ready { generation })
        {
            return Err(host_error(error_code::STALE));
        }
        Ok(stream)
    }

    /// 只接受路由器验证的进程代次；空集合取消订阅，避免旧请求改写新进程的注册。
    pub fn subscribe(
        &self,
        plugin: &PluginId,
        generation: u64,
        names: Vec<String>,
        cancellation: CancellationToken,
    ) -> Result<(), PluginHostError> {
        if cancellation.is_cancelled() {
            return Err(host_error(error_code::CANCELLED));
        }
        if names.len() > 128
            || names.iter().any(|name| {
                name.is_empty() || name.len() > 128 || name.chars().any(char::is_control)
            })
        {
            return Err(host_error(error_code::INVALID_PARAMS));
        }
        if self.process.state(plugin).ok() != Some(PluginRuntimeState::Ready { generation }) {
            return Err(host_error(error_code::STALE));
        }
        let mut subscriptions = self
            .subscriptions
            .lock()
            .map_err(|_| host_error(error_code::INTERNAL_ERROR))?;
        if cancellation.is_cancelled() {
            return Err(host_error(error_code::CANCELLED));
        }
        // 等锁期间进程可能已经重启，写入或取消前须再次核对原代次。
        if self.process.state(plugin).ok() != Some(PluginRuntimeState::Ready { generation }) {
            return Err(host_error(error_code::STALE));
        }
        if names.is_empty() {
            subscriptions.remove(plugin);
            return Ok(());
        }
        let entry = Arc::new(Subscription {
            generation,
            names: names.into_iter().collect(),
            cancellation,
        });
        subscriptions.insert(plugin.clone(), Arc::clone(&entry));
        drop(subscriptions);
        if entry.cancellation.is_cancelled() {
            self.remove_if_current(plugin, &entry);
            return Err(host_error(error_code::CANCELLED));
        }
        if self.process.state(plugin).ok() != Some(PluginRuntimeState::Ready { generation }) {
            self.remove_if_current(plugin, &entry);
            return Err(host_error(error_code::STALE));
        }
        Ok(())
    }

    /// 发布方只传可公开给插件的数据；失败如实返回，不能声称所有接收者均已收到。
    pub async fn publish(&self, name: &str, payload: Value, deadline: Instant) -> AppResult<usize> {
        self.publish_cancellable(name, payload, deadline, CancellationToken::new())
            .await
    }

    /// 窗口收尾必须中断正在等待的通知，并停止后续接收方；已投递的事件不回滚或重放。
    pub async fn publish_cancellable(
        &self,
        name: &str,
        payload: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<usize> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件事件发送已取消"));
        }
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return Err(AppError::invalid("插件事件名称无效"));
        }
        let targets = self
            .subscriptions
            .lock()
            .map_err(|_| AppError::infrastructure("插件事件订阅不可用"))?
            .iter()
            .map(|(plugin, entry)| (plugin.clone(), Arc::clone(entry)))
            .collect::<Vec<_>>();
        let mut delivered = 0;
        let mut failure = None;
        for (plugin, entry) in targets {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("插件事件发送已取消"));
            }
            if entry.cancellation.is_cancelled()
                || self.process.state(&plugin).ok()
                    != Some(PluginRuntimeState::Ready {
                        generation: entry.generation,
                    })
            {
                self.remove_if_current(&plugin, &entry);
                continue;
            }
            if !entry.names.contains(name) || !self.is_current(&plugin, &entry) {
                continue;
            }
            if Instant::now() >= deadline {
                failure.get_or_insert_with(|| AppError::timeout("插件事件发送"));
                break;
            }
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AppError::cancelled("插件事件发送已取消")),
                _ = entry.cancellation.cancelled() => continue,
                result = self.process.notify(
                    &plugin,
                    entry.generation,
                    "event",
                    json!({"name": name, "payload": payload}),
                    deadline,
                ) => result,
            };
            match result {
                Ok(()) => delivered += 1,
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件事件发送已取消"));
        }
        let pages = self
            .pages
            .lock()
            .map_err(|_| AppError::infrastructure("网页事件订阅不可用"))?
            .iter()
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect::<Vec<_>>();
        for (key, entry) in pages {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("插件事件发送已取消"));
            }
            let generation = entry.target.process_generation;
            let stale = entry.cancellation.is_cancelled()
                || (generation != 0
                    && self.process.state(&key.0.plugin).ok()
                        != Some(PluginRuntimeState::Ready { generation }));
            if stale {
                entry.cancellation.cancel();
                if let Ok(mut pages) = self.pages.lock()
                    && pages
                        .get(&key)
                        .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    pages.remove(&key);
                }
                continue;
            }
            if !entry.names.contains(name) {
                continue;
            }
            let message = json!({"name":name,"payload":payload});
            // 网页 IPC 还有身份信封；预留边界空间，超限必须明确失败。
            if serde_json::to_vec(&message).map_or(true, |bytes| bytes.len() > 60 * 1024) {
                failure.get_or_insert_with(|| AppError::invalid("网页事件超过大小限制"));
                continue;
            }
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AppError::cancelled("插件事件发送已取消")),
                _ = entry.cancellation.cancelled() => continue,
                result = tokio::time::timeout_at(deadline.into(), entry.sender.send(message)) => result,
            };
            match result {
                Ok(Ok(())) => delivered += 1,
                Ok(Err(_)) => {
                    entry.cancellation.cancel();
                    failure.get_or_insert_with(|| AppError::infrastructure("网页事件接收端已关闭"));
                }
                Err(_) => {
                    failure.get_or_insert_with(|| AppError::timeout("网页事件发送"));
                }
            }
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件事件发送已取消"));
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(delivered),
        }
    }

    /// 检查快照仍是当前订阅，避免取消或替换后再启动旧通知。
    fn is_current(&self, plugin: &PluginId, entry: &Arc<Subscription>) -> bool {
        self.subscriptions
            .lock()
            .ok()
            .and_then(|subscriptions| {
                subscriptions
                    .get(plugin)
                    .map(|current| Arc::ptr_eq(current, entry))
            })
            .unwrap_or(false)
    }

    /// 清理必须比较分配身份，迟到的旧代次不能删除同插件的新订阅。
    fn remove_if_current(&self, plugin: &PluginId, entry: &Arc<Subscription>) {
        if let Ok(mut subscriptions) = self.subscriptions.lock()
            && subscriptions
                .get(plugin)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            subscriptions.remove(plugin);
        }
    }
}
