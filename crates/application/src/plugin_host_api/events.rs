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
    plugin_ports::{PluginHostError, PluginProcessPort},
};

use super::host_error;

struct Subscription {
    generation: u64,
    names: BTreeSet<String>,
    cancellation: CancellationToken,
}

/// 订阅不持有进程或窗口实例；进程结束时令牌失效，旧订阅不能复用于重启后的代次。
pub struct PluginEventSubscriptions {
    process: Arc<dyn PluginProcessPort>,
    subscriptions: Mutex<BTreeMap<PluginId, Arc<Subscription>>>,
}

impl PluginEventSubscriptions {
    /// 与运行服务共享同一个监督器，通知始终按订阅时的代次发送。
    pub fn new(process: Arc<dyn PluginProcessPort>) -> Self {
        Self {
            process,
            subscriptions: Mutex::new(BTreeMap::new()),
        }
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
            let result = self
                .process
                .notify(
                    &plugin,
                    entry.generation,
                    "event",
                    json!({"name": name, "payload": payload}),
                    deadline,
                )
                .await;
            match result {
                Ok(()) => delivered += 1,
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
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
