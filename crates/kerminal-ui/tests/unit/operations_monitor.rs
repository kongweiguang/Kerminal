//! 运维服务器采样周期与分层快照合并契约测试。
//!
//! @author kongweiguang

use kerminal_domain::operations::{
    CpuSnapshot, DiskSnapshot, MemorySnapshot, ServerInfoProbeStatus, ServerInfoSampleTimes,
    ServerInfoSnapshot, ServerInfoTarget,
};

use super::super::ServerInfoRefreshInterval;
use super::{merge_server_info_snapshot, sampled_tier_expired, server_sample_is_immediate};
use tokio_util::sync::CancellationToken;

/// 隐藏时不再唤起采样，保留用户周期供重显后恢复；手动模式始终无周期。
///
/// @author kongweiguang
#[test]
fn hidden_panel_pauses_sampling_and_manual_disables_timer() {
    assert_eq!(
        ServerInfoRefreshInterval::OneSecond.period(true),
        Some(std::time::Duration::from_secs(1))
    );
    assert_eq!(ServerInfoRefreshInterval::OneSecond.period(false), None);
    assert_eq!(ServerInfoRefreshInterval::FiveMinutes.period(false), None);
    assert_eq!(
        ServerInfoRefreshInterval::FiveMinutes.period(true),
        Some(std::time::Duration::from_secs(300))
    );
    assert_eq!(ServerInfoRefreshInterval::Manual.period(true), None);
    assert_eq!(ServerInfoRefreshInterval::Manual.period(false), None);
}

/// 过期判断对缺失、未来和已到 TTL 的样本均保持可恢复行为。
#[test]
fn tier_expiry_handles_missing_future_and_elapsed_samples() {
    let ttl = std::time::Duration::from_secs(15);
    assert!(sampled_tier_expired(None, 20_000, ttl));
    assert!(sampled_tier_expired(Some(20_001), 20_000, ttl));
    assert!(!sampled_tier_expired(Some(10_000), 24_999, ttl));
    assert!(sampled_tier_expired(Some(10_000), 25_000, ttl));
}

/// 激活边沿版本在 driver 忙于旧采样时仍要求重显后立刻采样。
#[test]
fn reactivation_revision_forces_immediate_sample_after_missed_hide_edge() {
    let configuration = super::super::ServerMonitorConfiguration {
        revision: 8,
        server_generation: 3,
        immediate_revision: 5,
        panel_active: true,
        server_section_visible: true,
        interval: ServerInfoRefreshInterval::ThreeSeconds,
        wake: CancellationToken::new(),
    };

    assert!(server_sample_is_immediate(
        &configuration,
        Some(3),
        Some(4),
        true,
        true,
        Some(std::time::Instant::now()),
    ));
}

/// 配置刷新周期本身不强制额外探针，仍由下一次周期采样采用新间隔。
#[test]
fn interval_revision_does_not_force_an_extra_sample() {
    let configuration = super::super::ServerMonitorConfiguration {
        revision: 9,
        server_generation: 3,
        immediate_revision: 5,
        panel_active: true,
        server_section_visible: true,
        interval: ServerInfoRefreshInterval::FiveSeconds,
        wake: CancellationToken::new(),
    };

    assert!(!server_sample_is_immediate(
        &configuration,
        Some(3),
        Some(5),
        true,
        true,
        Some(std::time::Instant::now()),
    ));
}

/// 首次可见采样在 Manual 模式仍立即执行一次，避免把“无周期”误解为“永不首采”。
#[test]
fn missing_sample_requests_one_immediate_sample_in_manual_mode() {
    let configuration = super::super::ServerMonitorConfiguration {
        revision: 9,
        server_generation: 3,
        immediate_revision: 5,
        panel_active: true,
        server_section_visible: true,
        interval: ServerInfoRefreshInterval::Manual,
        wake: CancellationToken::new(),
    };

    assert!(server_sample_is_immediate(
        &configuration,
        Some(3),
        Some(5),
        true,
        true,
        None,
    ));
    assert_eq!(configuration.interval.period(true), None);
}

/// 采样后 Manual 模式没有周期且边沿未变化，因此 driver 必须进入等待而非立即重采。
#[test]
fn sampled_manual_configuration_does_not_request_another_sample() {
    let configuration = super::super::ServerMonitorConfiguration {
        revision: 9,
        server_generation: 3,
        immediate_revision: 5,
        panel_active: true,
        server_section_visible: true,
        interval: ServerInfoRefreshInterval::Manual,
        wake: CancellationToken::new(),
    };
    let last_sampled = Some(std::time::Instant::now());
    let immediate =
        server_sample_is_immediate(&configuration, Some(3), Some(5), true, true, last_sampled);
    let interval = configuration.interval.period(configuration.panel_active);
    let due = immediate
        || interval.is_some_and(|interval| {
            last_sampled.map_or(configuration.panel_active, |sampled| {
                sampled.elapsed() >= interval
            })
        });

    assert!(!immediate);
    assert_eq!(interval, None);
    assert!(!due);
}

/// 单层更新只替换已采样字段，未到期的 CPU/静态数据及原始采样时间保持真实值。
#[test]
fn slow_sample_preserves_fast_and_static_snapshot_fields() {
    let mut previous = snapshot(100);
    previous.hostname = Some("host-a".to_owned());
    previous.uptime_seconds = Some(7_200);
    previous.cpu.model = Some("cpu-a".to_owned());
    previous.cpu.usage_percent = Some(42.0);
    previous.memory.used_bytes = Some(100);
    previous.sample_times = ServerInfoSampleTimes {
        static_info_at_unix_ms: Some(50),
        fast_at_unix_ms: Some(100),
        slow_at_unix_ms: Some(200),
        ..ServerInfoSampleTimes::default()
    };

    let mut slow = snapshot(500);
    slow.sample_times.slow_at_unix_ms = Some(500);
    slow.disks.push(DiskSnapshot {
        filesystem: "filesystem-b".to_owned(),
        mount: "/".to_owned(),
        total_bytes: Some(2_000),
        used_bytes: Some(1_000),
        available_bytes: Some(1_000),
    });
    slow.disk_probe_status = ServerInfoProbeStatus::Available;

    let merged = merge_server_info_snapshot(Some(previous), slow);

    assert_eq!(merged.hostname.as_deref(), Some("host-a"));
    assert_eq!(merged.cpu.model.as_deref(), Some("cpu-a"));
    assert_eq!(merged.cpu.usage_percent, Some(42.0));
    assert_eq!(merged.memory.used_bytes, Some(100));
    assert_eq!(merged.uptime_seconds, Some(7_200));
    assert_eq!(merged.sample_times.static_info_at_unix_ms, Some(50));
    assert_eq!(merged.sample_times.fast_at_unix_ms, Some(100));
    assert_eq!(merged.sample_times.slow_at_unix_ms, Some(500));
    assert_eq!(merged.disks.len(), 1);
    assert_eq!(merged.disks[0].filesystem, "filesystem-b");
}

/// Fast 样本拥有 uptime 更新权；新的未知值清除旧值，避免把临时失效伪装成新鲜读数。
///
/// @author kongweiguang
#[test]
fn fast_sample_updates_and_clears_optional_uptime() {
    let mut previous = snapshot(100);
    previous.uptime_seconds = Some(900);
    previous.sample_times.fast_at_unix_ms = Some(100);

    let mut measured = snapshot(200);
    measured.sample_times.fast_at_unix_ms = Some(200);
    measured.uptime_seconds = Some(1_000);
    let merged = merge_server_info_snapshot(Some(previous), measured);
    assert_eq!(merged.uptime_seconds, Some(1_000));

    let mut unknown = snapshot(300);
    unknown.sample_times.fast_at_unix_ms = Some(300);
    let merged = merge_server_info_snapshot(Some(merged), unknown);
    assert_eq!(merged.uptime_seconds, None);
}

/// section 取消会立即使旧票据过期，避免回调将取消中的加载重新标成 Loading。
#[test]
fn section_cancellation_invalidates_the_old_request_ticket() {
    use super::super::state::{SectionPhase, SectionState};

    let mut section = SectionState::<u8>::new();
    let ticket = section.begin(7);
    section.cancel_pending();

    assert_eq!(section.phase(), SectionPhase::Cancelled);
    assert!(!section.is_current(&ticket));
}

/// 为字段合并契约测试构造没有虚假采样时间的最小快照。
///
/// @author kongweiguang
fn snapshot(captured_at_unix_ms: u64) -> ServerInfoSnapshot {
    ServerInfoSnapshot {
        target: ServerInfoTarget::Local,
        captured_at_unix_ms,
        sample_times: ServerInfoSampleTimes::default(),
        uptime_seconds: None,
        hostname: None,
        operating_system: None,
        architecture: None,
        kernel: None,
        cpu: CpuSnapshot::default(),
        memory: MemorySnapshot::default(),
        disks: Vec::new(),
        network_counters: Vec::new(),
        network_rates: Vec::new(),
        process_count: None,
        running_process_count: None,
        processes: Vec::new(),
        disk_probe_status: ServerInfoProbeStatus::NotSampled,
        network_probe_status: ServerInfoProbeStatus::NotSampled,
        process_probe_status: ServerInfoProbeStatus::NotSampled,
        gpu_probe_status: ServerInfoProbeStatus::NotSampled,
        gpus: Vec::new(),
        npu_probe_status: ServerInfoProbeStatus::NotSampled,
        npus: Vec::new(),
    }
}
