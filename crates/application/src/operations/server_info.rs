//! 主机资源快照用例与按目标/采样层级合并的只读缓存。
//!
//! 高成本采样经 collector port 执行；并发请求共享同一目标窗口，UI 不会重复启动探针。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_domain::operations::{
    NetworkCounterSnapshot, NetworkRateSnapshot, ServerInfoSnapshot, ServerInfoTarget,
    server_info::derive_network_rate,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

const MAX_CACHE_ENTRIES: usize = 24;
const MAX_NETWORK_RATE_BASELINES: usize = 24;
const MAX_NETWORK_RATE_SAMPLE_GAP: Duration = Duration::from_secs(120);

/// 查询采样层级；快速刷新不触发磁盘、进程或 GPU/NPU 的慢探针。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServerInfoTier {
    Fast,
    Slow,
    Static,
    Full,
}

/// 显式目标与所需采样范围；未指定目标时不会隐式选择当前 UI Tab。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ServerInfoQuery {
    pub target: ServerInfoTarget,
    pub tier: ServerInfoTier,
}

/// 基础设施采集器负责平台命令和协议解析，返回数据仍使用领域快照契约。
#[async_trait]
pub trait ServerInfoCollector: Send + Sync + 'static {
    /// 采集所需层级；实现必须响应取消并将不可用传感器留空而非填零。
    async fn collect(
        &self,
        target: ServerInfoTarget,
        tier: ServerInfoTier,
        cancel: CancellationToken,
    ) -> AppResult<ServerInfoSnapshot>;
}

/// 缓存值按 TTL 与采样层级分别管理，资源采样失效不会覆盖另一个层级。
#[derive(Clone)]
struct CachedSnapshot {
    snapshot: ServerInfoSnapshot,
    expires_at: Instant,
}

/// 保存最近一次同目标同层级计数；不同采样范围不能互相冒充前一帧。
#[derive(Clone)]
struct NetworkCounterBaseline {
    sampled_at: Instant,
    counters: Vec<NetworkCounterSnapshot>,
}

/// 只读服务器信息用例；缓存条目与 single-flight 锁都有固定上限。
#[derive(Clone)]
pub struct ServerInfoService {
    collector: Arc<dyn ServerInfoCollector>,
    cache: Arc<Mutex<HashMap<ServerInfoQuery, CachedSnapshot>>>,
    in_flight: Arc<Mutex<HashMap<ServerInfoQuery, Arc<Mutex<()>>>>>,
    network_baselines: Arc<Mutex<HashMap<ServerInfoQuery, NetworkCounterBaseline>>>,
}

impl ServerInfoService {
    /// 每个实例只依赖一个采集端口，避免 UI、MCP 各自构造重复轮询器。
    pub fn new(collector: Arc<dyn ServerInfoCollector>) -> Self {
        Self {
            collector,
            cache: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            network_baselines: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 缓存命中直接返回；miss 共享 per-key gate，并只用同目标同层级前帧推导速率。
    pub async fn snapshot(
        &self,
        query: ServerInfoQuery,
        cancel: CancellationToken,
    ) -> AppResult<ServerInfoSnapshot> {
        validate_query(&query)?;
        if let Some(snapshot) = self.cached(&query).await {
            return Ok(snapshot);
        }
        let gate = self.gate_for(&query).await;
        let guard = gate.lock().await;
        if let Some(snapshot) = self.cached(&query).await {
            drop(guard);
            self.release_gate(&query, &gate).await;
            return Ok(snapshot);
        }
        let result = self
            .collector
            .collect(query.target.clone(), query.tier, cancel)
            .await;
        let mut snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                drop(guard);
                self.release_gate(&query, &gate).await;
                return Err(error);
            }
        };
        if snapshot.target != query.target {
            drop(guard);
            self.release_gate(&query, &gate).await;
            return Err(AppError::infrastructure("资源采样目标与请求不一致"));
        }
        self.derive_network_rates(&query, &mut snapshot, Instant::now())
            .await;
        self.insert_cache(query.clone(), snapshot.clone()).await;
        drop(guard);
        self.release_gate(&query, &gate).await;
        Ok(snapshot)
    }

    /// 移除某目标缓存以触发新采样，同时保留上一成功计数供新帧比较。
    pub async fn invalidate(&self, target: &ServerInfoTarget) {
        self.cache
            .lock()
            .await
            .retain(|query, _| &query.target != target);
    }

    /// 速率历史按完整查询隔离；首帧、间隔过长和新接口只报告未知而不造零值。
    async fn derive_network_rates(
        &self,
        query: &ServerInfoQuery,
        snapshot: &mut ServerInfoSnapshot,
        sampled_at: Instant,
    ) {
        if snapshot.sample_times.fast_at_unix_ms.is_none() {
            snapshot.network_rates.clear();
            return;
        }
        let mut baselines = self.network_baselines.lock().await;
        let previous = baselines.get(query);
        snapshot.network_rates =
            calculate_network_rates(previous, &snapshot.network_counters, sampled_at);
        baselines.insert(
            query.clone(),
            NetworkCounterBaseline {
                sampled_at,
                counters: snapshot.network_counters.clone(),
            },
        );
        while baselines.len() > MAX_NETWORK_RATE_BASELINES {
            let oldest = baselines
                .iter()
                .min_by_key(|(_, baseline)| baseline.sampled_at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                baselines.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// 读取未过期快照并清除该 key 的过期项。
    async fn cached(&self, query: &ServerInfoQuery) -> Option<ServerInfoSnapshot> {
        let mut cache = self.cache.lock().await;
        match cache.get(query) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.snapshot.clone()),
            Some(_) => {
                cache.remove(query);
                None
            }
            None => None,
        }
    }

    /// 确保同一采样 key 只有一个 collector 调用。
    async fn gate_for(&self, query: &ServerInfoQuery) -> Arc<Mutex<()>> {
        let mut gates = self.in_flight.lock().await;
        gates
            .entry(query.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 缓存不超过固定目标层级数，优先淘汰最早过期快照。
    async fn insert_cache(&self, query: ServerInfoQuery, snapshot: ServerInfoSnapshot) {
        let mut cache = self.cache.lock().await;
        let expires_at = Instant::now() + cache_ttl(query.tier);
        cache.insert(
            query,
            CachedSnapshot {
                snapshot,
                expires_at,
            },
        );
        while cache.len() > MAX_CACHE_ENTRIES {
            let oldest = cache
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// 无 waiter 时移除 gate，避免历史 host ID 令同步表无限增长。
    async fn release_gate(&self, query: &ServerInfoQuery, gate: &Arc<Mutex<()>>) {
        let mut gates = self.in_flight.lock().await;
        let can_remove = gates
            .get(query)
            .is_some_and(|current| Arc::ptr_eq(current, gate) && Arc::strong_count(gate) == 2);
        if can_remove {
            gates.remove(query);
        }
    }
}

/// 两个 monotonic 样本间隔需有限且不超过窗口；网卡新增时仅该网卡速率未知。
fn calculate_network_rates(
    previous: Option<&NetworkCounterBaseline>,
    current: &[NetworkCounterSnapshot],
    sampled_at: Instant,
) -> Vec<NetworkRateSnapshot> {
    let previous = previous.filter(|baseline| {
        let elapsed = sampled_at.saturating_duration_since(baseline.sampled_at);
        !elapsed.is_zero() && elapsed <= MAX_NETWORK_RATE_SAMPLE_GAP
    });
    current
        .iter()
        .map(|counter| {
            previous
                .and_then(|baseline| {
                    baseline
                        .counters
                        .iter()
                        .find(|candidate| candidate.name == counter.name)
                        .map(|previous_counter| {
                            let elapsed = sampled_at
                                .saturating_duration_since(baseline.sampled_at)
                                .as_secs_f64();
                            derive_network_rate(previous_counter, counter, elapsed)
                        })
                })
                .unwrap_or_else(|| unknown_network_rate(counter))
        })
        .collect()
}

/// 新接口没有可比较的前值；None 明确表示尚未形成有效速率窗口。
fn unknown_network_rate(counter: &NetworkCounterSnapshot) -> NetworkRateSnapshot {
    NetworkRateSnapshot {
        name: counter.name.clone(),
        rx_bytes_per_second: None,
        tx_bytes_per_second: None,
        counter_reset: false,
    }
}

/// 将目标校验集中在用例入口，防止无效容器 ID 进入平台 collector。
fn validate_query(query: &ServerInfoQuery) -> AppResult<()> {
    if let ServerInfoTarget::Container { container_id, .. } = &query.target {
        let valid = !container_id.is_empty()
            && container_id.len() <= 128
            && container_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
            });
        if !valid {
            return Err(AppError::invalid("资源采样容器标识无效"));
        }
    }
    Ok(())
}

/// 快速采样共享短窗口，慢/静态数据采用旧产品明确的刷新周期。
fn cache_ttl(tier: ServerInfoTier) -> Duration {
    match tier {
        ServerInfoTier::Fast => Duration::from_millis(750),
        ServerInfoTier::Slow | ServerInfoTier::Full => Duration::from_secs(15),
        ServerInfoTier::Static => Duration::from_secs(300),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/server_info.rs"]
mod tests;
