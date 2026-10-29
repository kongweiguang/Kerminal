//! 运维服务器趋势投影保留缺失和无效采样的未知状态。
//!
//! @author kongweiguang

use super::super::super::ServerInfoRefreshInterval;
use super::super::super::state::{
    SectionPhase, SectionState, ServerInfoMetricSample, ServerInfoPanel,
};
use super::super::is_fixed_compact_operations_panel;
use super::SERVER_METRIC_PLOT_HEIGHT_PX;
use super::{
    ServerControlLabelMode, ServerNetworkFilter, ServerNetworkRole, ServerProcessSort,
    compare_server_processes, dynamic_rate_max, filtered_network_rates, network_interface_role,
    operations_target_label, percentage_label, preferred_host_display_name, server_bytes_label,
    server_control_label_mode, server_control_typography, server_device_count_label,
    server_disk_percent, server_gpu_utilization, server_page_title, server_panel_control_state,
    server_process_summary_label, server_processes_for_display, server_rate_label,
    server_refresh_disabled, server_root_disk, server_sampling_status,
    server_ssh_endpoint_subtitle, server_tab_height, server_target_label, server_target_subtitle,
    server_timestamp_label, server_uptime_label, should_render_server_feedback,
};
use crate::locale::UiLanguage;
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

/// 紧凑右栏的 Button 标签改用显式子节点，完整工作区继续使用 Kit 原生标签。
///
/// @author kongweiguang
#[test]
fn server_compact_controls_select_explicit_labels_only_in_compact_layout() {
    assert_eq!(
        server_control_label_mode(true),
        ServerControlLabelMode::StyledChild
    );
    assert_eq!(
        server_control_label_mode(false),
        ServerControlLabelMode::NativeButtonLabel
    );
    let typography = server_control_typography();
    assert_eq!(typography.font_size, gpui_kit::px(16.));
    assert_eq!(typography.line_height, gpui_kit::px(24.));
    assert_eq!(typography.font_weight, gpui_kit::FontWeight::NORMAL);
    assert_eq!(server_tab_height(true), gpui_kit::px(36.));
    assert_eq!(server_tab_height(false), gpui_kit::px(28.));
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
    assert!(["自动采集已暂停", "Automatic sampling paused"].contains(&manual.to_string().as_str()));
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

/// 离开资源/进程页才恢复各自默认值；同页操作不丢弃用户选择。
///
/// @author kongweiguang
#[test]
fn leaving_server_pages_resets_only_the_departing_panel_control() {
    let (network_filter, process_sort) = server_panel_control_state(
        ServerInfoPanel::Resources,
        ServerInfoPanel::Overview,
        ServerNetworkFilter::All,
        ServerProcessSort::Memory,
    );
    assert_eq!(network_filter, ServerNetworkFilter::Primary);
    assert_eq!(process_sort, ServerProcessSort::Memory);

    let (network_filter, process_sort) = server_panel_control_state(
        ServerInfoPanel::Processes,
        ServerInfoPanel::Resources,
        ServerNetworkFilter::Virtual,
        ServerProcessSort::Memory,
    );
    assert_eq!(network_filter, ServerNetworkFilter::Virtual);
    assert_eq!(process_sort, ServerProcessSort::Cpu);

    let (network_filter, process_sort) = server_panel_control_state(
        ServerInfoPanel::Resources,
        ServerInfoPanel::Resources,
        ServerNetworkFilter::Virtual,
        ServerProcessSort::Memory,
    );
    assert_eq!(network_filter, ServerNetworkFilter::Virtual);
    assert_eq!(process_sort, ServerProcessSort::Memory);
}

/// 以显式语言验证主分支计数投影与英文文案，避免读取并发共享的全局 locale。
///
/// @author kongweiguang
#[test]
fn process_summary_matches_the_main_count_projection() {
    assert_eq!(
        server_process_summary_label(None, None, UiLanguage::ZhCn),
        "-"
    );
    assert_eq!(
        server_process_summary_label(Some(15), None, UiLanguage::ZhCn),
        "15 个"
    );
    assert_eq!(
        server_process_summary_label(Some(15), None, UiLanguage::EnUs),
        "15"
    );
    assert_eq!(
        server_process_summary_label(Some(15), Some(14), UiLanguage::ZhCn),
        "15 个 / 运行 14"
    );
    assert_eq!(
        server_process_summary_label(Some(15), Some(14), UiLanguage::EnUs),
        "15 total / 14 running"
    );
}

/// 时间行显示本地绝对时刻，并将缺失或不可表示的毫秒值安全回退。
///
/// @author kongweiguang
#[test]
fn server_timestamp_label_is_absolute_and_rejects_out_of_range_values() {
    assert_eq!(server_timestamp_label(None), "-");
    assert_eq!(server_timestamp_label(Some(u64::MAX)), "-");
    let label = server_timestamp_label(Some(1_700_000_000_000));
    assert_eq!(label.len(), 8);
    assert_eq!(&label[2..3], ":");
    assert_eq!(&label[5..6], ":");
}

/// 远程副标题优先展示端点元数据；容器附加其运行时和精确 ID。
///
/// @author kongweiguang
#[test]
fn target_subtitle_prefers_endpoint_metadata_and_keeps_safe_fallbacks() {
    let host_id = HostId::new();
    let mut subtitles = HashMap::new();
    subtitles.insert(host_id.clone(), "root@192.0.2.1:22 · SSH".into());
    let target = ServerInfoTarget::Ssh {
        host_id: host_id.clone(),
    };
    assert_eq!(
        server_target_subtitle(&target, &subtitles, "Saved host"),
        "root@192.0.2.1:22"
    );
    assert_eq!(
        server_ssh_endpoint_subtitle("root@192.0.2.1:22 · SFTP"),
        "root@192.0.2.1:22 · SFTP"
    );
    assert_eq!(
        server_ssh_endpoint_subtitle("root@192.0.2.1:22 · SSH"),
        "root@192.0.2.1:22"
    );

    let container = ServerInfoTarget::Container {
        host_id: Some(host_id),
        runtime: ContainerRuntime::Docker,
        container_id: "container-123".to_owned(),
    };
    assert_eq!(
        server_target_subtitle(&container, &subtitles, "Container host"),
        "root@192.0.2.1:22 · SSH · Docker · container-123"
    );
    assert_eq!(
        server_target_subtitle(&container, &HashMap::new(), "Container host"),
        "Container host"
    );
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

/// 角色过滤在全接口排名后执行；Primary 保留所有物理网卡，无物理网卡时回退首个非回环接口。
///
/// @author kongweiguang
#[test]
fn network_filters_keep_physical_and_virtual_roles_distinct() {
    let rates = [
        network_rate("lo"),
        network_rate("eth2"),
        network_rate("docker0"),
        network_rate("ens18"),
        network_rate("tun0"),
        network_rate("veth123"),
    ];
    let counters = Vec::new();

    assert_eq!(network_interface_role("eth2"), ServerNetworkRole::Physical);
    assert_eq!(network_interface_role("docker0"), ServerNetworkRole::Bridge);
    assert_eq!(network_interface_role("tun0"), ServerNetworkRole::Tunnel);
    assert_eq!(
        network_interface_role("veth123"),
        ServerNetworkRole::Virtual
    );
    assert_eq!(network_interface_role("lo"), ServerNetworkRole::Loopback);
    assert_eq!(
        filtered_network_rates(&rates, &counters, ServerNetworkFilter::Primary)
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        ["eth2", "ens18"]
    );
    assert_eq!(
        filtered_network_rates(&rates, &counters, ServerNetworkFilter::All)
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        ["lo", "eth2", "docker0", "ens18", "tun0", "veth123"]
    );
    assert_eq!(
        filtered_network_rates(&rates, &counters, ServerNetworkFilter::Virtual)
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        ["docker0", "tun0", "veth123"]
    );

    let fallback = [network_rate("lo"), network_rate("docker0")];
    assert_eq!(
        filtered_network_rates(&fallback, &counters, ServerNetworkFilter::Primary)
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

/// 内存排序仍只重排 main 按 CPU 选出的前五项，不能从候选范围外引入进程。
///
/// @author kongweiguang
#[test]
fn process_memory_sort_keeps_the_main_cpu_top_five_scope() {
    let processes = vec![
        process(1, Some(100.0), Some(1)),
        process(2, Some(90.0), Some(2)),
        process(3, Some(80.0), Some(3)),
        process(4, Some(70.0), Some(4)),
        process(5, Some(60.0), Some(5)),
        process(6, Some(50.0), Some(1_000)),
    ];

    let by_memory = server_processes_for_display(&processes, ServerProcessSort::Memory);
    assert_eq!(
        by_memory
            .iter()
            .map(|process| process.pid)
            .collect::<Vec<_>>(),
        [5, 4, 3, 2, 1]
    );
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

/// 趋势绘制区与 main 的 28px Sparkline 一致，避免六项网格纵向膨胀。
///
/// @author kongweiguang
#[test]
fn overview_metric_plot_height_matches_main() {
    assert_eq!(SERVER_METRIC_PLOT_HEIGHT_PX, 28.0);
}

/// 网络行的空基线采用 main 的采样中占位，容量级别显示一致的 KB 到 TB 标签。
///
/// @author kongweiguang
#[test]
fn server_rate_labels_match_main_sampling_and_units() {
    assert_eq!(
        server_rate_label(None),
        crate::locale::text("采样中", "Sampling…").to_string()
    );
    assert_eq!(server_rate_label(Some(1_536.0)), "1.5 KB/s");
    assert_eq!(server_rate_label(Some(1_572_864.0)), "1.5 MB/s");
    assert_eq!(server_rate_label(Some(1_610_612_736.0)), "1.5 GB/s");
    assert_eq!(server_rate_label(Some(1_649_267_441_664.0)), "1.5 TB/s");
    assert_eq!(server_rate_label(Some(f64::NAN)), "—");
}

/// GPU 当前值只接受带时间的有效快照读数。
///
/// @author kongweiguang
#[test]
fn overview_metric_grid_and_gpu_projection_keep_real_data_constraints() {
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
