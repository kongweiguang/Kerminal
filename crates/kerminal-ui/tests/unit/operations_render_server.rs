//! 运维服务器趋势投影保留缺失和无效采样的未知状态。
//!
//! @author kongweiguang

use super::super::super::ServerInfoRefreshInterval;
use super::super::super::state::{SectionPhase, SectionState, ServerInfoMetricSample};
use super::super::{is_fixed_compact_operations_panel, should_suppress_ready_empty_feedback};
use super::SERVER_METRIC_PLOT_HEIGHT_PX;
use super::{
    SERVER_OVERVIEW_METRIC_MIN_COLUMN_WIDTH, ServerNetworkFilter, ServerNetworkRole,
    ServerProcessSort, compare_server_processes, dynamic_rate_max, filtered_network_rates,
    network_interface_role, operations_target_label, percentage_label, preferred_host_display_name,
    server_bytes_label, server_device_count_label, server_disk_percent, server_gpu_utilization,
    server_page_title, server_refresh_disabled, server_root_disk, server_sampling_status,
    server_target_label, server_uptime_label, should_render_server_feedback,
};
use kerminal_domain::identity::HostId;
use kerminal_domain::operations::{
    ContainerRuntime, DiskSnapshot, NetworkRateSnapshot, ProcessSnapshot, ServerInfoProbeFailure,
    ServerInfoProbeStatus, ServerInfoTarget,
};
use std::collections::HashMap;

/// 固定窄栏复用页头并摊平正文，完整工作台仍保留独立区块卡片。
///
/// @author kongweiguang
#[test]
fn fixed_compact_operation_page_flattens_its_nested_surface() {
    assert!(is_fixed_compact_operations_panel(true, true));
    assert!(!is_fixed_compact_operations_panel(true, false));
    assert!(!is_fixed_compact_operations_panel(false, true));
}

/// 服务器页头始终保留当前区块刷新；无目标和采样中两种情况禁用重复动作。
///
/// @author kongweiguang
#[test]
fn server_refresh_control_is_available_and_disabled_for_unsafe_states() {
    assert!(!server_refresh_disabled(SectionPhase::Ready, true));
    assert!(server_refresh_disabled(SectionPhase::Ready, false));
    assert!(server_refresh_disabled(SectionPhase::Loading, true));
}

/// 只有紧凑固定面板的成功空列表抑制通用提示，其他阶段继续保留原反馈语义。
///
/// @author kongweiguang
#[test]
fn successful_empty_forward_feedback_is_the_only_suppressed_phase() {
    assert!(should_suppress_ready_empty_feedback(
        SectionPhase::Ready,
        true,
        true
    ));
    assert!(!should_suppress_ready_empty_feedback(
        SectionPhase::Ready,
        true,
        false
    ));
    assert!(!should_suppress_ready_empty_feedback(
        SectionPhase::Ready,
        false,
        true
    ));
    for phase in [
        SectionPhase::NotLoaded,
        SectionPhase::Loading,
        SectionPhase::Failed,
        SectionPhase::Cancelled,
    ] {
        assert!(!should_suppress_ready_empty_feedback(phase, true, true));
    }
}

/// 页标题随精确目标类型变化，避免本机、SSH 与容器被混为同一泛化页面。
///
/// @author kongweiguang
#[test]
fn server_page_title_tracks_local_ssh_and_container_targets() {
    let local = ServerInfoTarget::Local;
    let ssh_host_id = HostId::new();
    let ssh = ServerInfoTarget::Ssh {
        host_id: ssh_host_id.clone(),
    };
    let container = ServerInfoTarget::Container {
        host_id: Some(ssh_host_id),
        runtime: ContainerRuntime::Docker,
        container_id: "container-123".to_owned(),
    };

    let local_title = server_page_title(&local).to_string();
    let ssh_title = server_page_title(&ssh).to_string();
    let container_title = server_page_title(&container).to_string();
    assert!(["本机服务器", "Local server"].contains(&local_title.as_str()));
    assert!(["远程服务器", "Remote server"].contains(&ssh_title.as_str()));
    assert!(["容器服务器", "Container server"].contains(&container_title.as_str()));
}

/// 标题保留保存名称原样，只有目录缺失或空白时才回退协议与稳定 ID。
///
/// @author kongweiguang
#[test]
fn operations_target_uses_saved_display_name_and_keeps_host_id_as_fallback() {
    let host_id = HostId::new();
    let target = ServerInfoTarget::Ssh {
        host_id: host_id.clone(),
    };
    let mut names = HashMap::new();
    names.insert(host_id.clone(), "r41 (隔离)".into());

    let display_name = preferred_host_display_name(&host_id, &names);
    assert_eq!(display_name, "r41 (隔离)");
    assert_eq!(operations_target_label(&host_id, &names), "r41 (隔离)");
    assert_eq!(
        server_target_label(&target, Some(display_name)),
        "r41 (隔离)"
    );
    assert_eq!(
        server_target_label(&target, None),
        format!("SSH · {}", host_id.as_str())
    );

    names.insert(host_id.clone(), "   ".into());
    assert_eq!(
        preferred_host_display_name(&host_id, &names),
        host_id.as_str()
    );
    assert_eq!(
        operations_target_label(&host_id, &names),
        format!("SSH · {}", host_id.as_str())
    );

    names.insert(host_id.clone(), "SSH · r41 隔离".into());
    assert_eq!(operations_target_label(&host_id, &names), "SSH · r41 隔离");
    assert_eq!(
        server_target_label(&target, Some("SSH · r41 隔离")),
        "SSH · r41 隔离"
    );
    let container = ServerInfoTarget::Container {
        host_id: Some(host_id),
        runtime: ContainerRuntime::Docker,
        container_id: "container-1234567890".to_owned(),
    };
    assert_eq!(
        server_target_label(&container, Some("r41 (隔离)")),
        "r41 (隔离) · Docker · container-1234567890"
    );
}

/// uptime 以紧凑时长呈现且未知继续为空值，不推导具有不同语义的开机时间。
///
/// @author kongweiguang
#[test]
fn uptime_label_keeps_unknown_and_compact_duration_semantics() {
    assert_eq!(server_uptime_label(None), "—");
    assert!(["45 秒", "45s"].contains(&server_uptime_label(Some(45)).as_str()));
    assert!(["1 天 1 小时", "1d 1h"].contains(&server_uptime_label(Some(90_061)).as_str()));
}

/// 手动刷新和周期采样显示不同状态，避免用户误以为手动模式仍在后台采集。
///
/// @author kongweiguang
#[test]
fn server_sampling_status_respects_manual_and_periodic_modes() {
    let manual = server_sampling_status(SectionPhase::Ready, ServerInfoRefreshInterval::Manual);
    let periodic =
        server_sampling_status(SectionPhase::Ready, ServerInfoRefreshInterval::ThreeSeconds);
    assert!(["采样完成", "Sampling complete"].contains(&manual.to_string().as_str()));
    assert!(["实时采集中", "Live sampling"].contains(&periodic.to_string().as_str()));
    let failed = server_sampling_status(SectionPhase::Failed, ServerInfoRefreshInterval::Manual);
    assert!(["采样失败", "Sampling failed"].contains(&failed.to_string().as_str()));
    let cancelled =
        server_sampling_status(SectionPhase::Cancelled, ServerInfoRefreshInterval::Manual);
    assert!(["采样已取消", "Sampling cancelled"].contains(&cancelled.to_string().as_str()));
    let waiting =
        server_sampling_status(SectionPhase::NotLoaded, ServerInfoRefreshInterval::Manual);
    assert!(["等待首次采样", "Waiting for first sample"].contains(&waiting.to_string().as_str()));
}

/// 设备数只有采样成功才渲染；失败或未采样不能被误读成零张卡。
///
/// @author kongweiguang
#[test]
fn device_count_remains_unknown_until_probe_completes() {
    let ready = server_device_count_label(
        0,
        "张",
        "devices",
        &ServerInfoProbeStatus::Available,
        Some(1_700_000_000_000),
    );
    assert!(["0 张", "0 devices"].contains(&ready.as_str()));

    let unknown =
        server_device_count_label(0, "张", "devices", &ServerInfoProbeStatus::NotSampled, None);
    assert!(["未采样", "Not sampled"].contains(&unknown.as_str()));
}

/// 主接口优先保留全部物理网卡；无物理网卡时回退到第一条非回环接口。
///
/// @author kongweiguang
#[test]
fn network_filters_keep_physical_and_virtual_roles_distinct() {
    let rates = [
        network_rate("lo"),
        network_rate("ens18"),
        network_rate("docker0"),
        network_rate("tun0"),
        network_rate("veth123"),
    ];

    assert_eq!(network_interface_role("ens18"), ServerNetworkRole::Physical);
    assert_eq!(network_interface_role("docker0"), ServerNetworkRole::Bridge);
    assert_eq!(network_interface_role("tun0"), ServerNetworkRole::Tunnel);
    assert_eq!(
        network_interface_role("veth123"),
        ServerNetworkRole::Virtual
    );
    assert_eq!(network_interface_role("lo"), ServerNetworkRole::Loopback);
    assert_eq!(
        filtered_network_rates(&rates, ServerNetworkFilter::Primary)
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        ["ens18"]
    );
    assert_eq!(
        filtered_network_rates(&rates, ServerNetworkFilter::All).len(),
        5
    );
    assert_eq!(
        filtered_network_rates(&rates, ServerNetworkFilter::Virtual)
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        ["docker0", "tun0", "veth123"]
    );

    let fallback = [network_rate("lo"), network_rate("docker0")];
    assert_eq!(
        filtered_network_rates(&fallback, ServerNetworkFilter::Primary)
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        ["docker0"]
    );
}

/// 进程表两种排序都将缺失值放在末尾，CPU 相同则用内存和 PID 给出稳定次序。
///
/// @author kongweiguang
#[test]
fn process_sorting_is_descending_stable_and_keeps_unknowns_last() {
    let processes = vec![
        process(31, None, None),
        process(21, Some(115.0), Some(14 * 1024 * 1024 * 1024)),
        process(22, Some(115.0), Some(13 * 1024 * 1024 * 1024)),
        process(11, Some(3.0), Some(15 * 1024 * 1024 * 1024)),
    ];

    let mut by_cpu = processes.clone();
    by_cpu.sort_by(|left, right| compare_server_processes(left, right, ServerProcessSort::Cpu));
    assert_eq!(
        by_cpu.iter().map(|process| process.pid).collect::<Vec<_>>(),
        [21, 22, 11, 31]
    );

    let mut by_memory = processes;
    by_memory
        .sort_by(|left, right| compare_server_processes(left, right, ServerProcessSort::Memory));
    assert_eq!(
        by_memory
            .iter()
            .map(|process| process.pid)
            .collect::<Vec<_>>(),
        [11, 21, 22, 31]
    );
    assert!(server_bytes_label(None) == "—");
    assert_eq!(server_bytes_label(Some(1024 * 1024 * 1024)), "1.0 GB");
}

/// 系统信息只将 POSIX/Windows 根路径当作根分区，并拒绝缺失或矛盾的容量百分比。
///
/// @author kongweiguang
#[test]
fn root_partition_uses_only_a_real_root_mount_and_valid_capacity() {
    let disks = [disk("/data", 20, 100, 20), disk("/", 35, 100, 57)];
    assert_eq!(
        server_root_disk(&disks).map(|disk| disk.mount.as_str()),
        Some("/")
    );
    assert_eq!(server_disk_percent(&disks[1]), Some(35.0));
    assert_eq!(server_root_disk(&[disk("/data", 20, 100, 20)]), None);
    assert_eq!(server_disk_percent(&disk("/", 110, 100, 0)), None);
}

/// 只创建本测试所需的速率字段，保持名称分类验证不受流量值影响。
///
/// @author kongweiguang
fn network_rate(name: &str) -> NetworkRateSnapshot {
    NetworkRateSnapshot {
        name: name.to_owned(),
        rx_bytes_per_second: None,
        tx_bytes_per_second: None,
        counter_reset: false,
    }
}

/// 创建可选字段完整的进程样本，以固定 CPU/内存排序与 PID 回退次序。
///
/// @author kongweiguang
fn process(pid: u32, cpu_percent: Option<f64>, memory_bytes: Option<u64>) -> ProcessSnapshot {
    ProcessSnapshot {
        pid,
        name: format!("process-{pid}"),
        cpu_percent,
        memory_percent: None,
        memory_bytes,
    }
}

/// 根挂载容量 fixture 显式提供 used/available，供矛盾边界回归使用。
///
/// @author kongweiguang
fn disk(mount: &str, used_bytes: u64, total_bytes: u64, available_bytes: u64) -> DiskSnapshot {
    DiskSnapshot {
        filesystem: "fixture".to_owned(),
        mount: mount.to_owned(),
        used_bytes: Some(used_bytes),
        total_bytes: Some(total_bytes),
        available_bytes: Some(available_bytes),
    }
}

/// 趋势绘制区至少 40px 保持窄栏可读，且不超过 64px 维持监控卡片的紧凑层级。
///
/// @author kongweiguang
#[test]
fn overview_metric_plot_height_remains_readable() {
    assert!((40.0..=64.0).contains(&SERVER_METRIC_PLOT_HEIGHT_PX));
}

/// 两列最小内容宽度受固定设计约束，GPU 当前值只接受带时间的有效快照读数。
///
/// @author kongweiguang
#[test]
fn overview_metric_grid_and_gpu_projection_keep_real_data_constraints() {
    assert_eq!(SERVER_OVERVIEW_METRIC_MIN_COLUMN_WIDTH, 136.0);

    let current = server_gpu_utilization(
        &ServerInfoProbeStatus::Available,
        Some(1_700_000_000_000),
        [Some(2.0), Some(4.0), None, Some(110.0), Some(f64::NAN)].into_iter(),
    );
    assert_eq!(current, Some(3.0));
    assert_eq!(
        server_gpu_utilization(
            &ServerInfoProbeStatus::NotSampled,
            Some(1_700_000_000_000),
            [Some(2.0)].into_iter(),
        ),
        None
    );
    assert_eq!(
        server_gpu_utilization(
            &ServerInfoProbeStatus::Available,
            None,
            [Some(2.0)].into_iter(),
        ),
        None
    );
    assert_eq!(
        server_gpu_utilization(
            &ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::TimedOut),
            Some(1_700_000_000_000),
            [Some(2.0)].into_iter(),
        ),
        None
    );
    assert_eq!(
        server_gpu_utilization(
            &ServerInfoProbeStatus::Available,
            Some(1_700_000_000_000),
            [None, Some(110.0), Some(f64::INFINITY)].into_iter(),
        ),
        None
    );
}

/// 成功与后台采集都不挂载反馈子项，避免空 div 增加 gap；失败、取消仍保留反馈和旧快照。
///
/// @author kongweiguang
#[test]
fn server_refresh_keeps_snapshot_without_loading_feedback() {
    let mut state = SectionState::<u64>::new();
    let initial = state.begin(1);
    assert!(should_render_server_feedback(&state));
    assert!(state.complete(&initial, Ok(42)));
    assert!(!should_render_server_feedback(&state));

    let refresh = state.begin(1);
    assert!(!should_render_server_feedback(&state));

    assert!(state.complete(&refresh, Ok(43)));
    assert!(!should_render_server_feedback(&state));

    let refresh = state.begin(1);
    assert!(state.cancel(&refresh));
    assert!(should_render_server_feedback(&state));

    let refresh = state.begin(1);
    assert!(!should_render_server_feedback(&state));

    assert!(state.complete(&refresh, Err("sample failed".to_owned())));
    assert!(should_render_server_feedback(&state));
    assert_eq!(state.data(), Some(&43));
    assert_eq!(state.error(), Some("sample failed"));
}

/// 只有真实且非负的有限速率可形成观测量程；底层安全绘图量程不能冒充观测值。
///
/// @author kongweiguang
#[test]
fn missing_or_invalid_rates_have_unknown_range() {
    let samples = [
        sample(None),
        sample(Some(-1.0)),
        sample(Some(f64::NAN)),
        sample(Some(f64::INFINITY)),
    ];

    assert_eq!(dynamic_rate_max(&[]), None);
    assert_eq!(dynamic_rate_max(&samples), None);
}

/// 越界或非有限的百分比保持未知，避免趋势标签把坏样本伪装成有效读数。
///
/// @author kongweiguang
#[test]
fn invalid_percentages_remain_unknown() {
    for value in [
        None,
        Some(-0.1),
        Some(100.1),
        Some(f64::NAN),
        Some(f64::INFINITY),
    ] {
        assert_eq!(percentage_label(value), "—");
    }
    assert_eq!(percentage_label(Some(42.0)), "42.0%");
}

/// 全零流量仍是有效观测，并使用最小安全量程供 PlotElement 绘制。
///
/// @author kongweiguang
#[test]
fn valid_zero_rate_uses_minimum_plot_range() {
    assert_eq!(dynamic_rate_max(&[sample(Some(0.0))]), Some(1.0));
}

/// 构造单个时间序列点，保持测试输入只覆盖速率观测值。
///
/// @author kongweiguang
fn sample(value: Option<f64>) -> ServerInfoMetricSample {
    ServerInfoMetricSample {
        sampled_at_unix_ms: 1,
        value,
    }
}
