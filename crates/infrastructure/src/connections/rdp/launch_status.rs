//! RDP 启动状态的有界内存目录；状态跟随现有 launcher owner task 发布。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};

use kerminal_application::{
    AppError, AppResult,
    connections::{
        RdpClientLifecycle,
        rdp_launch_status::{
            RdpLaunchId, RdpLaunchStatus, RdpLaunchStatusMessage, RdpLaunchStatusService,
            RdpLaunchStatusSubscription,
        },
    },
};
use tokio::sync::watch;

/// 有界状态目录；活动 launch 不会被淘汰，容量不足时拒绝新的 RDP 启动。
pub struct RdpLaunchStatusHub {
    capacity: usize,
    terminal_retention: usize,
    registry: Mutex<Registry>,
}

struct Registry {
    entries: HashMap<RdpLaunchId, StatusEntry>,
    terminal_order: VecDeque<RdpLaunchId>,
}

struct StatusEntry {
    current: RdpLaunchStatusMessage,
    sender: watch::Sender<RdpLaunchStatusMessage>,
}

/// 只交给原有客户端 owner task 的状态发布句柄，subscriber 取消不会影响该句柄。
#[derive(Clone)]
pub struct RdpLaunchStatusReporter {
    launch_id: RdpLaunchId,
    lifecycle: RdpClientLifecycle,
    hub: Weak<RdpLaunchStatusHub>,
}

impl RdpLaunchStatusHub {
    /// 限制活动与终态记录总量，防止长时间运行积累启动快照和 watcher。
    pub fn new(capacity: usize, terminal_retention: usize) -> AppResult<Self> {
        if capacity == 0 || terminal_retention == 0 || terminal_retention > capacity {
            return Err(AppError::invalid("RDP 启动状态保留上限无效"));
        }
        Ok(Self {
            capacity,
            terminal_retention,
            registry: Mutex::new(Registry {
                entries: HashMap::new(),
                terminal_order: VecDeque::new(),
            }),
        })
    }

    /// 在 spawn 前登记 Requested，确保容量不足时不会遗留未受管的客户端进程。
    pub fn register(
        self: &Arc<Self>,
        lifecycle: RdpClientLifecycle,
    ) -> AppResult<(RdpLaunchId, RdpLaunchStatusReporter)> {
        let mut registry = self.lock_registry()?;
        while registry.entries.len() >= self.capacity {
            if !evict_oldest_terminal(&mut registry) {
                return Err(AppError::conflict("RDP 启动状态目录繁忙，请稍后重试"));
            }
        }

        let launch_id = RdpLaunchId::new();
        let message = RdpLaunchStatusMessage {
            launch_id,
            revision: 0,
            status: RdpLaunchStatus::Requested,
        };
        let (sender, _receiver) = watch::channel(message);
        registry.entries.insert(
            launch_id,
            StatusEntry {
                current: message,
                sender,
            },
        );
        Ok((
            launch_id,
            RdpLaunchStatusReporter {
                launch_id,
                lifecycle,
                hub: Arc::downgrade(self),
            },
        ))
    }

    /// 将终态数限制在保留上限内，按终态产生顺序淘汰最旧记录。
    fn trim_terminal_records(&self, registry: &mut Registry) {
        while registry.terminal_order.len() > self.terminal_retention {
            let _ = evict_oldest_terminal(registry);
        }
    }

    /// Poisoned registry 不恢复可能不一致的 lifecycle 状态，改为向调用方报告受控错误。
    fn lock_registry(&self) -> AppResult<std::sync::MutexGuard<'_, Registry>> {
        self.registry
            .lock()
            .map_err(|_| AppError::infrastructure("RDP 启动状态目录不可用"))
    }
}

impl RdpLaunchStatusReporter {
    /// 只允许 Requested 单向转入与真实平台生命周期匹配的一个终态。
    pub fn publish(&self, status: RdpLaunchStatus) -> AppResult<RdpLaunchStatusMessage> {
        if !status.is_terminal() || !status_matches_lifecycle(status, self.lifecycle) {
            return Err(AppError::conflict("RDP 启动状态转换无效"));
        }
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| AppError::infrastructure("RDP 启动状态目录已关闭"))?;
        let mut registry = hub.lock_registry()?;
        let entry = registry
            .entries
            .get_mut(&self.launch_id)
            .ok_or_else(|| AppError::not_found("RDP 启动状态已过期"))?;
        if entry.current.status.is_terminal() {
            return Err(AppError::conflict("RDP 启动状态已进入终态"));
        }
        let revision = entry
            .current
            .revision
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("RDP 启动状态版本已耗尽"))?;
        let message = RdpLaunchStatusMessage {
            launch_id: self.launch_id,
            revision,
            status,
        };
        entry.current = message;
        entry.sender.send_replace(message);
        registry.terminal_order.push_back(self.launch_id);
        hub.trim_terminal_records(&mut registry);
        Ok(message)
    }
}

impl RdpLaunchStatusService for RdpLaunchStatusHub {
    /// 仅按确切 launch ID 查询，目录保留窗口外统一返回脱敏 NotFound。
    fn query(&self, launch_id: RdpLaunchId) -> AppResult<RdpLaunchStatusMessage> {
        let registry = self.lock_registry()?;
        registry
            .entries
            .get(&launch_id)
            .map(|entry| entry.current)
            .ok_or_else(|| AppError::not_found("RDP 启动状态已过期"))
    }

    /// 为单个消费者克隆 watch receiver；不会把订阅的取消传播给 launcher owner。
    fn subscribe(&self, launch_id: RdpLaunchId) -> AppResult<RdpLaunchStatusSubscription> {
        let registry = self.lock_registry()?;
        let receiver = registry
            .entries
            .get(&launch_id)
            .map(|entry| entry.sender.subscribe())
            .ok_or_else(|| AppError::not_found("RDP 启动状态已过期"))?;
        Ok(RdpLaunchStatusSubscription::from_receiver(receiver))
    }
}

/// 只有当前启动请求且其客户端类型相符时才可产生终态，禁止把委派打开伪报为关闭。
fn status_matches_lifecycle(status: RdpLaunchStatus, lifecycle: RdpClientLifecycle) -> bool {
    match status {
        RdpLaunchStatus::DelegatedToSystem => lifecycle == RdpClientLifecycle::DelegatedToSystem,
        RdpLaunchStatus::Closed => lifecycle == RdpClientLifecycle::ManagedProcess,
        RdpLaunchStatus::Failed { .. } => true,
        RdpLaunchStatus::Requested => false,
    }
}

/// 删除终态队列中最旧且仍存在的记录，忽略此前因容量收缩而失效的队列 ID。
fn evict_oldest_terminal(registry: &mut Registry) -> bool {
    while let Some(launch_id) = registry.terminal_order.pop_front() {
        let is_terminal = registry
            .entries
            .get(&launch_id)
            .is_some_and(|entry| entry.current.status.is_terminal());
        if is_terminal {
            registry.entries.remove(&launch_id);
            return true;
        }
    }
    false
}
