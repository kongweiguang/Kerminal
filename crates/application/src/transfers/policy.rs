//! SFTP 入队预算快照与共享 admission gate；活跃任务持 permit，更新只影响新任务快照。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use kerminal_domain::identity::HostId;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult, preferences::SftpPerformanceSettings};

const MIN_GLOBAL_TRANSFERS: usize = 1;
const MAX_GLOBAL_TRANSFERS: usize = 16;
const MIN_HOST_TRANSFERS: usize = 1;
const MAX_HOST_TRANSFERS: usize = 8;
const MIN_PIPELINE_DEPTH: usize = 1;
const MAX_PIPELINE_DEPTH: usize = 256;
const MIN_PACKET_BYTES: u32 = 32 * 1024;
const MAX_PACKET_BYTES: u32 = 256 * 1024;
const MIN_IDLE_TIMEOUT_SECONDS: u16 = 30;
const MAX_IDLE_TIMEOUT_SECONDS: u16 = 3_600;

/// 入队时复制的不可变 SFTP 参数；包括准入上限，后续配置更新不改写已有任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SftpTransferPolicySnapshot {
    global_transfers: usize,
    host_transfers: usize,
    pipeline_depth: usize,
    packet_bytes: u32,
    idle_timeout_seconds: u16,
}

impl SftpTransferPolicySnapshot {
    /// 严格验证持久化设置，避免将越界配置静默夹取后误报为用户实际选择。
    pub fn from_settings(settings: SftpPerformanceSettings) -> AppResult<Self> {
        if !(MIN_GLOBAL_TRANSFERS..=MAX_GLOBAL_TRANSFERS).contains(&settings.global_transfers)
            || !(MIN_HOST_TRANSFERS..=MAX_HOST_TRANSFERS).contains(&settings.host_transfers)
            || settings.host_transfers > settings.global_transfers
            || !(MIN_PIPELINE_DEPTH..=MAX_PIPELINE_DEPTH).contains(&settings.pipeline_depth)
            || !(MIN_PACKET_BYTES..=MAX_PACKET_BYTES).contains(&settings.packet_bytes)
            || !(MIN_IDLE_TIMEOUT_SECONDS..=MAX_IDLE_TIMEOUT_SECONDS)
                .contains(&settings.idle_timeout_seconds)
        {
            return Err(AppError::invalid("SFTP 传输策略超出支持范围"));
        }

        Ok(Self {
            global_transfers: settings.global_transfers,
            host_transfers: settings.host_transfers,
            pipeline_depth: settings.pipeline_depth,
            packet_bytes: settings.packet_bytes,
            idle_timeout_seconds: settings.idle_timeout_seconds,
        })
    }

    /// 返回该任务入队时捕获的全局并发上限。
    pub const fn global_transfers(self) -> usize {
        self.global_transfers
    }

    /// 返回该任务入队时捕获的每主机并发上限，跨主机传输会同时占用两端预算。
    pub const fn host_transfers(self) -> usize {
        self.host_transfers
    }

    /// 返回单次 SFTP 写入流水线允许的请求数。
    pub const fn pipeline_depth(self) -> usize {
        self.pipeline_depth
    }

    /// 返回协议 packet 上限；实际读写仍受文件余量及对端能力限制。
    pub const fn packet_bytes(self) -> u32 {
        self.packet_bytes
    }

    /// 返回连续无确认进度阈值，不把它解释为整次任务的总时长。
    pub const fn idle_timeout(self) -> Duration {
        Duration::from_secs(self.idle_timeout_seconds as u64)
    }

    /// 计算单任务最大在途窗口，输入范围保证乘法不会溢出。
    pub const fn pipeline_window_bytes(self) -> u64 {
        self.pipeline_depth as u64 * self.packet_bytes as u64
    }
}

/// 一个应用共享的可变策略入口；配置快照可各自冻结，但所有 SFTP 任务共用活动计数。
#[derive(Clone)]
pub struct SftpTransferPolicyStore {
    inner: Arc<PolicyStoreInner>,
}

struct PolicyStoreInner {
    state: Mutex<PolicyStoreState>,
    changed: Notify,
}

struct PolicyStoreState {
    snapshot: SftpTransferPolicySnapshot,
    active_transfers: usize,
    active_by_host: HashMap<HostId, usize>,
    closed: bool,
}

/// 有效传输持有的 RAII 预算；Drop 只更新进程内计数，不执行异步 I/O。
pub struct SftpTransferPermit {
    inner: Arc<PolicyStoreInner>,
    hosts: Vec<HostId>,
}

impl SftpTransferPolicyStore {
    /// 初始设置在建服务时验证一次，避免以默认值掩盖损坏或未来版本配置。
    pub fn new(settings: SftpPerformanceSettings) -> AppResult<Self> {
        let snapshot = SftpTransferPolicySnapshot::from_settings(settings)?;
        Ok(Self {
            inner: Arc::new(PolicyStoreInner {
                state: Mutex::new(PolicyStoreState {
                    snapshot,
                    active_transfers: 0,
                    active_by_host: HashMap::new(),
                    closed: false,
                }),
                changed: Notify::new(),
            }),
        })
    }

    /// 原子读取当前参数，调用方可把副本绑定到新入队任务。
    pub fn snapshot(&self) -> SftpTransferPolicySnapshot {
        lock_state(&self.inner).snapshot
    }

    /// 一次性替换新任务默认策略；已有任务的快照和 permit 保留，唤醒等待者重查各自门槛。
    pub fn update(
        &self,
        settings: SftpPerformanceSettings,
    ) -> AppResult<SftpTransferPolicySnapshot> {
        let snapshot = SftpTransferPolicySnapshot::from_settings(settings)?;
        {
            let mut state = lock_state(&self.inner);
            if state.closed {
                return Err(AppError::cancelled("SFTP 传输策略服务已关闭"));
            }
            state.snapshot = snapshot;
        }
        self.inner.changed.notify_waiters();
        Ok(snapshot)
    }

    /// 用当前策略等待全局及主机预算同时可用，保留 store 级动态准入能力。
    pub async fn acquire(
        &self,
        hosts: impl IntoIterator<Item = HostId>,
        cancellation: &CancellationToken,
    ) -> AppResult<SftpTransferPermit> {
        self.acquire_with_host_limits(hosts.into_iter().map(|host| (host, None)), cancellation)
            .await
    }

    /// 用当前策略原子准入所有主机；队列 worker 应使用其入队快照版本。
    pub async fn acquire_with_host_limits(
        &self,
        hosts: impl IntoIterator<Item = (HostId, Option<usize>)>,
        cancellation: &CancellationToken,
    ) -> AppResult<SftpTransferPermit> {
        self.acquire_with_host_limits_inner(hosts, None, cancellation)
            .await
    }

    /// 排队任务始终用入队快照判断准入，设置更新只改变之后入队任务的限额。
    ///
    /// @author kongweiguang
    pub(crate) async fn acquire_with_host_limits_at_snapshot(
        &self,
        hosts: impl IntoIterator<Item = (HostId, Option<usize>)>,
        snapshot: SftpTransferPolicySnapshot,
        cancellation: &CancellationToken,
    ) -> AppResult<SftpTransferPermit> {
        self.acquire_with_host_limits_inner(hosts, Some(snapshot), cancellation)
            .await
    }

    /// 同步门闩始终使用共享计数；可选快照只改变此 worker 的阈值，避免按配置版本分裂配额。
    ///
    /// @author kongweiguang
    async fn acquire_with_host_limits_inner(
        &self,
        hosts: impl IntoIterator<Item = (HostId, Option<usize>)>,
        admission_snapshot: Option<SftpTransferPolicySnapshot>,
        cancellation: &CancellationToken,
    ) -> AppResult<SftpTransferPermit> {
        let mut limits = std::collections::BTreeMap::new();
        for (host, limit) in hosts {
            if limit.is_some_and(|value| !(1..=8).contains(&value)) {
                return Err(AppError::invalid("主机 SFTP 并发覆盖必须在 1 至 8 之间"));
            }
            limits
                .entry(host)
                .and_modify(|existing: &mut Option<usize>| {
                    *existing = match (*existing, limit) {
                        (Some(left), Some(right)) => Some(left.min(right)),
                        (Some(value), None) | (None, Some(value)) => Some(value),
                        (None, None) => None,
                    };
                })
                .or_insert(limit);
        }

        loop {
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut state = lock_state(&self.inner);
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled("等待 SFTP 传输预算时已取消"));
                }
                if state.closed {
                    return Err(AppError::cancelled("SFTP 传输策略服务已关闭"));
                }
                let snapshot = admission_snapshot.unwrap_or(state.snapshot);
                if state.active_transfers < snapshot.global_transfers
                    && limits.iter().all(|(host, limit)| {
                        state.active_by_host.get(host).copied().unwrap_or_default()
                            < limit
                                .unwrap_or(snapshot.host_transfers)
                                .min(snapshot.global_transfers)
                    })
                {
                    state.active_transfers += 1;
                    for host in limits.keys() {
                        *state.active_by_host.entry(host.clone()).or_default() += 1;
                    }
                    return Ok(SftpTransferPermit {
                        inner: self.inner.clone(),
                        hosts: limits.into_keys().collect(),
                    });
                }
            }

            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(AppError::cancelled("等待 SFTP 传输预算时已取消"));
                }
                _ = notified => {}
            }
        }
    }

    /// 停止新传输并唤醒所有等候者；已有传输仍由 permit owner 正常释放。
    pub fn close(&self) {
        lock_state(&self.inner).closed = true;
        self.inner.changed.notify_waiters();
    }
}

impl SftpTransferPermit {
    /// 返回该 permit 一并占用的去重主机集合，便于诊断而不暴露凭据。
    pub fn hosts(&self) -> &[HostId] {
        &self.hosts
    }
}

impl Drop for SftpTransferPermit {
    /// 释放全局与主机计数并唤醒候选任务；计数在同一锁中回收以保持配额一致。
    fn drop(&mut self) {
        let mut state = lock_state(&self.inner);
        state.active_transfers = state.active_transfers.saturating_sub(1);
        for host in &self.hosts {
            let remove_host = if let Some(active) = state.active_by_host.get_mut(host) {
                *active = active.saturating_sub(1);
                *active == 0
            } else {
                false
            };
            if remove_host {
                state.active_by_host.remove(host);
            }
        }
        drop(state);
        self.inner.changed.notify_waiters();
    }
}

/// 同步临界区不跨 await；Mutex poison 时 fail closed，避免损坏计数继续准入新任务。
fn lock_state(inner: &PolicyStoreInner) -> MutexGuard<'_, PolicyStoreState> {
    match inner.state.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            let mut state = PoisonError::into_inner(poisoned);
            state.closed = true;
            inner.changed.notify_waiters();
            state
        }
    }
}
