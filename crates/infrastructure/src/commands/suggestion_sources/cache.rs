//! 远端建议结果的限时缓存与同键刷新门禁。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex as StdMutex, Weak},
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, OwnedMutexGuard};

/// 单个缓存值的有效期和 LRU 访问时刻。
struct CacheEntry<V> {
    expires_at: Instant,
    last_access: Instant,
    value: Arc<V>,
}

/// 为缓存 miss 建立独立 gate，避免同一 host/path 的输入按键触发重复远端查询。
pub(super) struct BoundedAsyncCache<K, V> {
    capacity: usize,
    ttl: Duration,
    entries: Mutex<HashMap<K, CacheEntry<V>>>,
    failures: Mutex<HashMap<K, Instant>>,
    gates: StdMutex<HashMap<K, Weak<Mutex<()>>>>,
    overflow_gate: Arc<Mutex<()>>,
}

impl<K, V> BoundedAsyncCache<K, V>
where
    K: Clone + Eq + Hash,
{
    /// 按固定容量和 TTL 创建进程内缓存，禁止缓存对象无界增长或持久化远端快照。
    pub(super) fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            entries: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            gates: StdMutex::new(HashMap::new()),
            overflow_gate: Arc::new(Mutex::new(())),
        }
    }

    /// 返回尚未过期的缓存副本，并刷新 LRU 时刻，不持锁跨越网络或文件访问。
    pub(super) async fn get(&self, key: &K) -> Option<Arc<V>> {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        entries.retain(|_, entry| entry.expires_at > now);
        entries.get_mut(key).map(|entry| {
            entry.last_access = now;
            Arc::clone(&entry.value)
        })
    }

    /// 清理空闲弱引用并限制 gate 表；超限的新键共用一个回退 gate 保持并发有界。
    pub(super) fn gate(&self, key: &K) -> Arc<Mutex<()>> {
        let mut gates = lock_recover(&self.gates);
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
            return gate;
        }
        if gates.len() >= self.capacity {
            return Arc::clone(&self.overflow_gate);
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(key.clone(), Arc::downgrade(&gate));
        gate
    }

    /// 将成功发现的只读快照写入有界 LRU，替换相同键并及时释放最旧的内容。
    pub(super) async fn insert(&self, key: K, value: V) {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        entries.retain(|_, entry| entry.expires_at > now);
        entries.insert(
            key.clone(),
            CacheEntry {
                expires_at: now + self.ttl,
                last_access: now,
                value: Arc::new(value),
            },
        );
        while entries.len() > self.capacity {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                break;
            }
        }
        drop(entries);
        self.failures.lock().await.remove(&key);
    }

    /// 记录短暂失败退避，防止一个不可达主机在每次按键时都重新建连。
    pub(super) async fn mark_failure(&self, key: K, backoff: Duration) {
        let now = Instant::now();
        let mut failures = self.failures.lock().await;
        failures.retain(|_, expires_at| *expires_at > now);
        failures.insert(key, now + backoff);
        while failures.len() > self.capacity {
            let oldest = failures
                .iter()
                .min_by_key(|(_, expires_at)| **expires_at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                failures.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// 判断指定键是否仍在失败退避期，过期记录会在读取时及时移除。
    pub(super) async fn is_backing_off(&self, key: &K) -> bool {
        let now = Instant::now();
        let mut failures = self.failures.lock().await;
        failures.retain(|_, expires_at| *expires_at > now);
        failures.contains_key(key)
    }
}

/// 在 mutex 中毒时回收已有 gate；缓存元数据无领域状态，继续服务剩余请求更稳妥。
fn lock_recover<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 以取消优先的方式等待同键刷新 gate，避免等待者阻塞退出或切换 session。
pub(super) async fn lock_gate(
    gate: Arc<Mutex<()>>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<OwnedMutexGuard<()>> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        guard = gate.lock_owned() => Some(guard),
    }
}
