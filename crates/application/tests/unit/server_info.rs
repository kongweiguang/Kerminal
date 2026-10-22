//! 主机网络速率按目标和采样层级隔离，未知窗口不伪造零速率。
//!
//! @author kongweiguang

use super::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use kerminal_domain::operations::{
    CpuSnapshot, MemorySnapshot, NetworkCounterSnapshot, ServerInfoProbeStatus,
    ServerInfoSampleTimes,
};

/// 首次读取、新网卡和采样间隔超限都没有可比较的速率窗口。
#[test]
fn network_rate_is_unknown_without_a_valid_previous_counter() {
    let now = Instant::now();
    let baseline = NetworkCounterBaseline {
        sampled_at: now,
        counters: vec![counter("eth0", 100, 200)],
    };

    assert!(
        calculate_network_rates(None, &[counter("eth0", 110, 220)], now)
            .iter()
            .all(|rate| rate.rx_bytes_per_second.is_none())
    );
    assert!(
        calculate_network_rates(
            Some(&baseline),
            &[counter("wlan0", 110, 220)],
            now + Duration::from_secs(1),
        )[0]
        .rx_bytes_per_second
        .is_none()
    );
    assert!(
        calculate_network_rates(
            Some(&baseline),
            &[counter("eth0", 110, 220)],
            now + MAX_NETWORK_RATE_SAMPLE_GAP + Duration::from_millis(1),
        )[0]
        .rx_bytes_per_second
        .is_none()
    );
}

/// 单调计数按真实单调时间计算，计数回退时只标记 reset 而不返回负速率。
#[test]
fn network_rate_uses_elapsed_time_and_marks_counter_reset() {
    let now = Instant::now();
    let baseline = NetworkCounterBaseline {
        sampled_at: now,
        counters: vec![counter("eth0", 100, 200)],
    };
    let rates = calculate_network_rates(
        Some(&baseline),
        &[counter("eth0", 130, 260)],
        now + Duration::from_secs(10),
    );
    assert_eq!(rates[0].rx_bytes_per_second, Some(3.0));
    assert_eq!(rates[0].tx_bytes_per_second, Some(6.0));
    assert!(!rates[0].counter_reset);

    let reset = calculate_network_rates(
        Some(&baseline),
        &[counter("eth0", 90, 210)],
        now + Duration::from_secs(10),
    );
    assert!(reset[0].counter_reset);
    assert!(reset[0].rx_bytes_per_second.is_none());
    assert!(reset[0].tx_bytes_per_second.is_none());
}

/// 不同 tier 的首帧分别未知，Fast 的下一帧仍只比较 Fast 自己的上一帧。
#[tokio::test]
async fn server_info_baselines_are_isolated_by_tier() {
    let collector = Arc::new(CounterCollector::new([
        vec![counter("eth0", 100, 100)],
        vec![counter("eth0", 900, 900)],
        vec![counter("eth0", 300, 300)],
    ]));
    let service = ServerInfoService::new(collector);
    let target = ServerInfoTarget::Local;
    let fast = ServerInfoQuery {
        target: target.clone(),
        tier: ServerInfoTier::Fast,
    };
    let full = ServerInfoQuery {
        target: target.clone(),
        tier: ServerInfoTier::Full,
    };

    let first_fast = service
        .snapshot(fast.clone(), CancellationToken::new())
        .await
        .expect("first fast sample should be available");
    assert!(first_fast.network_rates[0].rx_bytes_per_second.is_none());
    service.invalidate(&target).await;
    let first_full = service
        .snapshot(full, CancellationToken::new())
        .await
        .expect("first full sample should be available");
    assert!(first_full.network_rates[0].rx_bytes_per_second.is_none());
    service.invalidate(&target).await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    let second_fast = service
        .snapshot(fast, CancellationToken::new())
        .await
        .expect("second fast sample should be available");
    assert!(second_fast.network_rates[0].rx_bytes_per_second.is_some());
}

/// 只保留当前解析器需要的网卡计数，避免每个测试重复构造领域数据。
fn counter(name: &str, rx_bytes: u64, tx_bytes: u64) -> NetworkCounterSnapshot {
    NetworkCounterSnapshot {
        name: name.to_owned(),
        rx_bytes,
        tx_bytes,
    }
}

/// 让 service 测试可以逐次提供明确计数样本，而不访问宿主机资源。
struct CounterCollector {
    samples: Mutex<VecDeque<Vec<NetworkCounterSnapshot>>>,
}

impl CounterCollector {
    /// 测试替身只接受固定样本序列，耗尽时失败可见，不生成默认计数。
    fn new(samples: impl IntoIterator<Item = Vec<NetworkCounterSnapshot>>) -> Self {
        Self {
            samples: Mutex::new(samples.into_iter().collect()),
        }
    }
}

#[async_trait]
impl ServerInfoCollector for CounterCollector {
    /// 仅回放一个网络计数快照，隔离对墙钟采样和系统接口的依赖。
    ///
    /// @author kongweiguang
    async fn collect(
        &self,
        target: ServerInfoTarget,
        _tier: ServerInfoTier,
        _cancel: CancellationToken,
    ) -> AppResult<ServerInfoSnapshot> {
        let counters = self
            .samples
            .lock()
            .expect("sample mutex poisoned")
            .pop_front()
            .ok_or_else(|| AppError::infrastructure("no test network sample remains"))?;
        Ok(ServerInfoSnapshot {
            target,
            captured_at_unix_ms: 1,
            sample_times: ServerInfoSampleTimes {
                fast_at_unix_ms: Some(1),
                ..ServerInfoSampleTimes::default()
            },
            uptime_seconds: None,
            hostname: None,
            operating_system: None,
            architecture: None,
            kernel: None,
            cpu: CpuSnapshot::default(),
            memory: MemorySnapshot::default(),
            disks: Vec::new(),
            network_counters: counters,
            network_rates: Vec::new(),
            process_count: None,
            running_process_count: None,
            processes: Vec::new(),
            disk_probe_status: ServerInfoProbeStatus::NotSampled,
            network_probe_status: ServerInfoProbeStatus::Available,
            process_probe_status: ServerInfoProbeStatus::NotSampled,
            gpu_probe_status: ServerInfoProbeStatus::NotSampled,
            gpus: Vec::new(),
            npu_probe_status: ServerInfoProbeStatus::NotSampled,
            npus: Vec::new(),
        })
    }
}
