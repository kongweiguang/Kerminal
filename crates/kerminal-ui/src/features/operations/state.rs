//! 运维各能力区块的独立读取状态与过期请求防护。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
};

use kerminal_application::ApplicationTaskExecutor;
use kerminal_domain::{
    identity::HostId,
    operations::{
        ComposeProjectTarget, ContainerAction, ContainerRuntime, ContainerStatus, ContainerSummary,
        ContainerTarget, NetworkCounterSnapshot, NetworkRateSnapshot, ServerInfoProbeStatus,
        ServerInfoSnapshot, ServerInfoTarget, TmuxSession,
    },
};
use tokio_util::sync::CancellationToken;

use super::OperationsSection;

const SERVER_HISTORY_CAPACITY: usize = 60;

/// 只比较会话身份，客户端数、活动时间和工作目录变化不能把正常确认变成过期确认。
/// 创建时间与 target_ref 一并核对，防止 tmux server 重启后复用原 ID。
/// @author kongweiguang
pub(super) fn tmux_confirmation_session_matches(
    expected: &TmuxSession,
    current: Option<&TmuxSession>,
) -> bool {
    current.is_some_and(|current| {
        expected.id == current.id
            && expected.name == current.name
            && expected.created_at == current.created_at
            && expected.target_ref == current.target_ref
    })
}

/// 服务器详情只切换展示投影，不重复探测或丢弃同一目标的采样历史。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ServerInfoPanel {
    #[default]
    Overview,
    Resources,
    Processes,
}

/// 资源页和概览历史共享接口范围，默认聚焦主链路并保留显式扩展筛选。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::features::operations) enum ServerNetworkFilter {
    #[default]
    Primary,
    All,
    Virtual,
}

/// 网络接口按名称归类，未知命名保持为物理接口以免隐藏定制网卡。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServerNetworkRole {
    Bridge,
    Loopback,
    Physical,
    Tunnel,
    Virtual,
}

/// 将 Linux 常见桥接、回环、隧道和虚拟接口前缀映射到展示角色。
///
/// @author kongweiguang
pub(super) fn network_interface_role(name: &str) -> ServerNetworkRole {
    let name = name.to_ascii_lowercase();
    if name == "lo" {
        ServerNetworkRole::Loopback
    } else if ["br-", "bridge", "docker", "virbr"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        ServerNetworkRole::Bridge
    } else if ["tun", "tap", "tailscale", "wg", "zt"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        ServerNetworkRole::Tunnel
    } else if ["veth", "cni", "flannel", "kube-ipvs", "dummy"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        ServerNetworkRole::Virtual
    } else {
        ServerNetworkRole::Physical
    }
}

/// 先按 main 的速率或计数排名全部接口，再筛选角色；无物理接口时取排名最高的非回环接口。
///
/// @author kongweiguang
pub(super) fn filtered_network_rates<'a>(
    rates: &'a [NetworkRateSnapshot],
    counters: &[NetworkCounterSnapshot],
    filter: ServerNetworkFilter,
) -> Vec<&'a NetworkRateSnapshot> {
    let has_rate_sample = rates
        .iter()
        .any(|rate| rate.rx_bytes_per_second.is_some() || rate.tx_bytes_per_second.is_some());
    let counter_totals = counters
        .iter()
        .map(|counter| {
            (
                counter.name.as_str(),
                counter.rx_bytes as f64 + counter.tx_bytes as f64,
            )
        })
        .collect::<HashMap<_, _>>();
    let score = |rate: &NetworkRateSnapshot| {
        if has_rate_sample {
            rate.rx_bytes_per_second.unwrap_or(0.0) + rate.tx_bytes_per_second.unwrap_or(0.0)
        } else {
            counter_totals
                .get(rate.name.as_str())
                .copied()
                .unwrap_or(0.0)
        }
    };
    let mut ranked = rates.iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        score(right)
            .partial_cmp(&score(left))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    match filter {
        ServerNetworkFilter::All => ranked,
        ServerNetworkFilter::Virtual => ranked
            .into_iter()
            .filter(|rate| {
                matches!(
                    network_interface_role(&rate.name),
                    ServerNetworkRole::Bridge
                        | ServerNetworkRole::Tunnel
                        | ServerNetworkRole::Virtual
                )
            })
            .collect(),
        ServerNetworkFilter::Primary => {
            let physical = ranked
                .iter()
                .copied()
                .filter(|rate| network_interface_role(&rate.name) == ServerNetworkRole::Physical)
                .collect::<Vec<_>>();
            if physical.is_empty() {
                ranked
                    .into_iter()
                    .filter(|rate| {
                        network_interface_role(&rate.name) != ServerNetworkRole::Loopback
                    })
                    .take(1)
                    .collect()
            } else {
                physical
            }
        }
    }
}

/// 容器状态筛选使用领域状态，不从本地化 status 文本推断运行态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ContainerStatusFilter {
    #[default]
    All,
    Running,
}

/// 已确认的容器变更请求是 UI 与 Root live-context gate 之间的单一参数快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerMutationRequest {
    pub(super) target: ContainerTarget,
    pub(super) action: ContainerAction,
    pub(super) force: bool,
}

/// 历史点只保存 Fast 层真实采样值；缺失字段保持 None，不能被画成零。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ServerInfoHistorySample {
    pub(super) sampled_at_unix_ms: u64,
    pub(super) cpu_percent: Option<f64>,
    pub(super) memory_percent: Option<f64>,
    pub(super) receive_bytes_per_second: Option<f64>,
    pub(super) transmit_bytes_per_second: Option<f64>,
}

/// 慢速资源序列保存真实层级时间；无效或缺失读数作为断点而非零值。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ServerInfoMetricSample {
    pub(super) sampled_at_unix_ms: u64,
    pub(super) value: Option<f64>,
}

/// 每个面板实例保存自己的有界采样队列，目标变化会清除旧目标曲线。
#[derive(Default)]
pub(super) struct ServerInfoHistory {
    target: Option<ServerInfoTarget>,
    samples: VecDeque<ServerInfoHistorySample>,
    disk_samples: VecDeque<ServerInfoMetricSample>,
    gpu_samples: VecDeque<ServerInfoMetricSample>,
    disk_mount: Option<String>,
    last_disk_at_unix_ms: Option<u64>,
    last_gpu_at_unix_ms: Option<u64>,
}

/// Compose 项目分组保留容器原始顺序，未带项目标签的容器单独归类。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ContainerProjectGroup {
    pub(super) project: Option<String>,
    pub(super) compose_group: bool,
    pub(super) target: Option<ComposeProjectTarget>,
    pub(super) containers: Vec<ContainerSummary>,
}

impl ServerInfoHistory {
    /// 各层级只在自身采样时间前进时新增点；概览网络曲线复用 Primary 接口及主分支回退规则。
    ///
    /// @author kongweiguang
    pub(super) fn record(&mut self, snapshot: &ServerInfoSnapshot) -> bool {
        if self.target.as_ref() != Some(&snapshot.target) {
            self.target = Some(snapshot.target.clone());
            self.samples.clear();
            self.disk_samples.clear();
            self.gpu_samples.clear();
            self.disk_mount = None;
            self.last_disk_at_unix_ms = None;
            self.last_gpu_at_unix_ms = None;
        }
        let mut recorded = false;
        if let Some(sampled_at_unix_ms) = snapshot.sample_times.fast_at_unix_ms
            && self
                .samples
                .back()
                .is_none_or(|sample| sample.sampled_at_unix_ms < sampled_at_unix_ms)
        {
            let memory_percent = match (snapshot.memory.used_bytes, snapshot.memory.total_bytes) {
                (Some(used), Some(total)) if total > 0 => {
                    Some((used as f64 * 100.0 / total as f64).clamp(0.0, 100.0))
                }
                _ => None,
            };
            let (receive_bytes_per_second, transmit_bytes_per_second) =
                primary_network_rate_totals(&snapshot.network_rates, &snapshot.network_counters);
            self.samples.push_back(ServerInfoHistorySample {
                sampled_at_unix_ms,
                cpu_percent: bounded_percent(snapshot.cpu.usage_percent),
                memory_percent,
                receive_bytes_per_second,
                transmit_bytes_per_second,
            });
            trim_history(&mut self.samples);
            recorded = true;
        }

        if let Some(sampled_at_unix_ms) = snapshot.sample_times.slow_at_unix_ms
            && self
                .last_disk_at_unix_ms
                .is_none_or(|last| last < sampled_at_unix_ms)
        {
            self.last_disk_at_unix_ms = Some(sampled_at_unix_ms);
            let disk = if snapshot.disk_probe_status == ServerInfoProbeStatus::Available {
                snapshot
                    .disks
                    .iter()
                    .find(|disk| disk.mount == "/")
                    .or_else(|| snapshot.disks.first())
            } else {
                None
            };
            if let Some(disk) = disk
                && self.disk_mount.as_deref() != Some(disk.mount.as_str())
            {
                // 不把不同挂载点的容量拼成一条曲线；当前卡片始终对应同一文件系统。
                self.disk_samples.clear();
                self.disk_mount = Some(disk.mount.clone());
            }
            let value = disk.and_then(|disk| {
                let used = disk.used_bytes?;
                let total = disk.total_bytes?;
                (total > 0).then(|| bounded_percent(Some(used as f64 * 100.0 / total as f64)))?
            });
            self.disk_samples.push_back(ServerInfoMetricSample {
                sampled_at_unix_ms,
                value,
            });
            trim_history(&mut self.disk_samples);
            recorded = true;
        }

        if let Some(sampled_at_unix_ms) = snapshot.sample_times.gpu_at_unix_ms
            && self
                .last_gpu_at_unix_ms
                .is_none_or(|last| last < sampled_at_unix_ms)
        {
            self.last_gpu_at_unix_ms = Some(sampled_at_unix_ms);
            let value = if snapshot.gpu_probe_status == ServerInfoProbeStatus::Available {
                let readings = snapshot
                    .gpus
                    .iter()
                    .filter_map(|gpu| bounded_percent(gpu.utilization_percent))
                    .collect::<Vec<_>>();
                (!readings.is_empty()).then(|| readings.iter().sum::<f64>() / readings.len() as f64)
            } else {
                None
            };
            self.gpu_samples.push_back(ServerInfoMetricSample {
                sampled_at_unix_ms,
                value,
            });
            trim_history(&mut self.gpu_samples);
            recorded = true;
        }
        recorded
    }

    /// 只有仍在加载的当前票据才能同时完成快照并写入曲线，迟到结果不污染历史目标。
    ///
    /// @author kongweiguang
    pub(super) fn complete_if_current(
        &mut self,
        section: &mut SectionState<ServerInfoSnapshot>,
        ticket: &LoadTicket,
        snapshot: ServerInfoSnapshot,
    ) -> bool {
        if !section.is_current(ticket) {
            return false;
        }
        self.record(&snapshot);
        section.complete(ticket, Ok(snapshot))
    }

    /// 返回真实历史点，渲染层只负责呈现且不能改写采样时间。
    ///
    /// @author kongweiguang
    pub(super) fn samples(&self) -> &VecDeque<ServerInfoHistorySample> {
        &self.samples
    }
}

/// 只有位于物理百分比范围内的读数进入趋势；越界数据作为断点而不被夹成边界值。
///
/// @author kongweiguang
fn bounded_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

/// 汇总有限非负读数，跳过单接口缺测；该方向完全未知时保留趋势断点。
///
/// @author kongweiguang
fn sum_network_rates(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0;
    for value in values
        .flatten()
        .filter(|value| value.is_finite() && *value >= 0.0)
    {
        sum += value;
        if !sum.is_finite() {
            return None;
        }
        count += 1;
    }
    (count > 0).then_some(sum)
}

/// 只汇总 Primary 接口读数；某个方向缺测时保留空值，避免其它角色流量冒充主链路。
///
/// @author kongweiguang
fn primary_network_rate_totals(
    rates: &[NetworkRateSnapshot],
    counters: &[NetworkCounterSnapshot],
) -> (Option<f64>, Option<f64>) {
    let primary = filtered_network_rates(rates, counters, ServerNetworkFilter::Primary);
    let receive = sum_network_rates(primary.iter().map(|rate| rate.rx_bytes_per_second));
    let transmit = sum_network_rates(primary.iter().map(|rate| rate.tx_bytes_per_second));
    (receive, transmit)
}

/// 让各层级曲线共享有界窗口，不以循环刷新次数推导真实时间。
///
/// @author kongweiguang
fn trim_history<T>(samples: &mut VecDeque<T>) {
    while samples.len() > SERVER_HISTORY_CAPACITY {
        samples.pop_front();
    }
}

/// 将时间戳和量程内数据映射到 0–1 绘图区；缺失或越界样本由调用方作为抬笔断点处理。
///
/// @author kongweiguang
pub(super) fn time_series_position(
    sampled_at_unix_ms: u64,
    value: f64,
    start_at_unix_ms: u64,
    end_at_unix_ms: u64,
    minimum: f64,
    maximum: f64,
) -> Option<(f64, f64)> {
    if !value.is_finite()
        || value < minimum
        || value > maximum
        || !minimum.is_finite()
        || !maximum.is_finite()
        || maximum <= minimum
    {
        return None;
    }
    let x = if end_at_unix_ms <= start_at_unix_ms {
        0.5
    } else {
        sampled_at_unix_ms.saturating_sub(start_at_unix_ms) as f64
            / end_at_unix_ms.saturating_sub(start_at_unix_ms) as f64
    };
    let y = 1.0 - (value - minimum) / (maximum - minimum);
    Some((x.clamp(0.0, 1.0), y))
}

/// 速率沿用 main 的字节换算与单位标签；首个快照尚无差分时明确显示采样中。
///
/// @author kongweiguang
pub(super) fn network_rate_label(value: Option<f64>) -> String {
    let Some(value) = value else {
        return crate::locale::text("采样中", "Sampling…").to_string();
    };
    if !value.is_finite() || value < 0.0 {
        return "—".to_owned();
    }
    if value >= 1_099_511_627_776.0 {
        format!("{:.1} TB/s", value / 1_099_511_627_776.0)
    } else if value >= 1_073_741_824.0 {
        format!("{:.1} GB/s", value / 1_073_741_824.0)
    } else if value >= 1_048_576.0 {
        format!("{:.1} MB/s", value / 1_048_576.0)
    } else if value >= 1024.0 {
        format!("{:.1} KB/s", value / 1024.0)
    } else {
        format!("{value:.0} B/s")
    }
}

/// 按领域运行状态和实际可搜索字段筛选容器；“已停止”只包含可辨认的终态/未启动态。
///
/// @author kongweiguang
pub(super) fn filter_containers(
    containers: &[ContainerSummary],
    query: &str,
    status_filter: ContainerStatusFilter,
) -> Vec<ContainerSummary> {
    let query = query.trim().to_lowercase();
    containers
        .iter()
        .filter(|container| match status_filter {
            ContainerStatusFilter::All => true,
            ContainerStatusFilter::Running => container.status == ContainerStatus::Running,
        })
        .filter(|container| {
            if query.is_empty() {
                return true;
            }

            let matches = |candidate: &str| candidate.to_lowercase().contains(&query);
            let runtime = match container.target.runtime {
                ContainerRuntime::Docker => "docker",
                ContainerRuntime::Podman => "podman",
            };
            [
                container.name.as_str(),
                container.short_id.as_str(),
                container.target.container_id.as_str(),
                container.image.as_str(),
                container.status_text.as_str(),
                runtime,
            ]
            .into_iter()
            .any(matches)
                || container.ports.iter().any(|port| matches(port))
                || container.compose.as_ref().is_some_and(|compose| {
                    compose
                        .project
                        .iter()
                        .chain(compose.service.iter())
                        .chain(compose.working_dir.iter())
                        .chain(compose.config_files.iter())
                        .chain(compose.config_paths.iter())
                        .any(|candidate| matches(candidate))
                })
        })
        .cloned()
        .collect()
}

/// 从同一行标签构造完整 Compose 目标；缺字段时不合并项目，也不合成执行范围。
///
/// @author kongweiguang
pub(super) fn compose_target(container: &ContainerSummary) -> Option<ComposeProjectTarget> {
    let metadata = container.compose.as_ref()?;
    Some(ComposeProjectTarget {
        host_id: container.target.host_id.clone(),
        runtime: container.target.runtime,
        project: metadata.project.clone()?,
        working_dir: metadata.working_dir.clone()?,
        // config_files 的原始顺序可能表达叠加语义，因此既不排序也不归一化。
        config_files: (!metadata.config_files.is_empty()).then(|| metadata.config_files.clone())?,
    })
}

/// 将容器按完整 ComposeProjectTarget 精确分组；不完整元数据逐项保留，standalone 仍聚合。
///
/// @author kongweiguang
pub(super) fn group_container_projects(
    containers: Vec<ContainerSummary>,
) -> Vec<ContainerProjectGroup> {
    let mut groups: Vec<ContainerProjectGroup> = Vec::new();
    for container in containers {
        let compose_group = container.compose.is_some();
        let target = compose_target(&container);
        let project = target
            .as_ref()
            .map(|target| target.project.clone())
            .or_else(|| {
                container
                    .compose
                    .as_ref()
                    .and_then(|compose| compose.project.clone())
            });
        let existing_group = if let Some(target) = target.as_ref() {
            groups
                .iter_mut()
                .find(|group| group.target.as_ref() == Some(target))
        } else if !compose_group {
            groups.iter_mut().find(|group| !group.compose_group)
        } else {
            // 项目名相同但工作目录或配置文件缺失时无法证明执行目标相同，保持独立行组。
            None
        };
        if let Some(group) = existing_group {
            group.containers.push(container);
        } else {
            groups.push(ContainerProjectGroup {
                project,
                compose_group,
                target,
                containers: vec![container],
            });
        }
    }
    groups
}

/// 停止或拆除后仍按完整目标补回空项目；同名不同目录、配置顺序或主机绝不合并。
pub(super) fn group_container_projects_with_retained(
    containers: Vec<ContainerSummary>,
    projects: &[ComposeProjectTarget],
) -> Vec<ContainerProjectGroup> {
    let mut groups = group_container_projects(containers);
    for target in projects {
        if !groups
            .iter()
            .any(|group| group.target.as_ref() == Some(target))
        {
            groups.push(ContainerProjectGroup {
                project: Some(target.project.clone()),
                compose_group: true,
                target: Some(target.clone()),
                containers: Vec::new(),
            });
        }
    }
    groups
}

/// 单独嵌入 ToolRail 的面板锁定自己的 section；完整工作台仍可自由切换。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SectionSelection {
    active: OperationsSection,
    fixed: bool,
}

impl SectionSelection {
    /// 完整工作台默认显示服务器区，并允许导航改变选择。
    pub(super) const fn dashboard() -> Self {
        Self {
            active: OperationsSection::ServerInfo,
            fixed: false,
        }
    }

    /// 独立面板锁定到指定区块，避免被其它 Rail 槽位中的视图状态影响。
    pub(super) const fn fixed(section: OperationsSection) -> Self {
        Self {
            active: section,
            fixed: true,
        }
    }

    /// 只允许完整工作台切换 section，返回值表示活动内容确实改变。
    pub(super) fn select(&mut self, section: OperationsSection) -> bool {
        if self.fixed || self.active == section {
            return false;
        }
        self.active = section;
        true
    }

    /// 返回当前绘制的能力区块。
    pub(super) const fn active(self) -> OperationsSection {
        self.active
    }

    /// 判断实例是否是固定区块面板。
    pub(super) const fn is_fixed(self) -> bool {
        self.fixed
    }

    /// 固定面板的刷新只命中自身区块；完整工作台使用 `None` 表示刷新全部。
    pub(super) const fn refresh_scope(self) -> Option<OperationsSection> {
        if self.fixed { Some(self.active) } else { None }
    }

    /// 转发状态订阅只由完整工作台或专属 Ports 面板持有。
    pub(super) const fn watches_forwards(self) -> bool {
        !self.fixed || matches!(self.active, OperationsSection::PortForwards)
    }
}

/// 每个 section 独立加载，避免 tmux 或容器失败遮蔽其他可用监控数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SectionPhase {
    /// 目标尚未读取。
    NotLoaded,
    /// 请求正在执行；已有快照可继续展示并标注为旧数据。
    Loading,
    /// 最近一次读取成功。
    Ready,
    /// 最近一次读取失败；旧快照仍保留供用户识别和重试。
    Failed,
    /// 当前查询被取消；旧快照保留，空结果仍不冒充成功。
    Cancelled,
}

/// 当前 section 的请求票据；取消令牌让目标切换和重复刷新可协作终止旧请求。
#[derive(Clone)]
pub(super) struct LoadTicket {
    /// 发起请求时选中的目标世代。
    pub(super) target_generation: u64,
    /// 同一目标下每次刷新递增的请求序号。
    pub(super) request_id: u64,
    /// 由 section 持有并在新请求/目标切换时取消。
    pub(super) cancellation: CancellationToken,
}

/// section 内的数据与失败状态分离，失败时不把未能读取误报成空结果。
pub(super) struct SectionState<T> {
    data: Option<T>,
    phase: SectionPhase,
    error: Option<String>,
    target_generation: u64,
    request_id: u64,
    cancellation: Option<CancellationToken>,
}

/// 共享详情面板只接受最近一次点击的回执，并在新请求到来时取消旧操作。
pub(super) struct LatestRequestState {
    request_id: u64,
    cancellation: Option<CancellationToken>,
}

/// 单次详情请求的票据；取消令牌同时传入 application 用例。
#[derive(Clone)]
pub(super) struct LatestRequestTicket {
    request_id: u64,
    pub(super) cancellation: CancellationToken,
}

/// 记录视图离开后可能已变更的明确目标，返回时必须重新读取权威快照。
#[derive(Default)]
pub(super) struct DirtyOperationTargets {
    containers: Vec<(Option<HostId>, ContainerRuntime)>,
    tmux_hosts: Vec<Option<HostId>>,
}

/// tmux 回执分开决定主机级列表刷新与 pane 级状态更新，避免新 pane 丢失旧主机的脏列表。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TmuxMutationDisposition {
    pub(super) refresh_current_host: bool,
    pub(super) update_current_view: bool,
}

impl DirtyOperationTargets {
    /// 将迟到容器回执绑定原 host/runtime；只有当前语义目标相同才允许立即刷新。
    pub(super) fn container_mutation_completed(
        &mut self,
        target_host: Option<HostId>,
        target_runtime: ContainerRuntime,
        current_host: &Option<HostId>,
        current_runtime: ContainerRuntime,
    ) -> bool {
        self.mark_container_dirty(target_host.clone(), target_runtime);
        target_host.as_ref() == current_host.as_ref() && target_runtime == current_runtime
    }

    /// 记录容器运行时可能被外部终端修改，等待对应目标的权威读取确认状态。
    pub(super) fn mark_container_dirty(
        &mut self,
        host_id: Option<HostId>,
        runtime: ContainerRuntime,
    ) {
        if !self
            .containers
            .iter()
            .any(|(host, candidate)| host == &host_id && *candidate == runtime)
        {
            self.containers.push((host_id, runtime));
        }
    }

    /// 同一容器目标重新读取成功后才清除脏标记，失败快照不会被当成已核对。
    pub(super) fn container_refreshed(
        &mut self,
        host_id: &Option<HostId>,
        runtime: ContainerRuntime,
    ) {
        self.containers
            .retain(|(host, candidate)| host != host_id || *candidate != runtime);
    }

    /// 将迟到 tmux 回执绑定原 host；同 host 仍刷新列表，pane 状态仅接受原视图代次。
    ///
    /// @author kongweiguang
    pub(super) fn tmux_mutation_completed(
        &mut self,
        target_host: Option<HostId>,
        current_host: &Option<HostId>,
        mutation_view_generation: u64,
        current_view_generation: u64,
    ) -> TmuxMutationDisposition {
        self.mark_tmux_dirty(target_host.clone());
        let refresh_current_host = target_host.as_ref() == current_host.as_ref();
        TmuxMutationDisposition {
            refresh_current_host,
            update_current_view: refresh_current_host
                && mutation_view_generation == current_view_generation,
        }
    }

    /// 记录由 tmux 外部命令产生的可能变化，等待对应 host 的权威读取确认。
    pub(super) fn mark_tmux_dirty(&mut self, host_id: Option<HostId>) {
        if !self.tmux_hosts.iter().any(|host| host == &host_id) {
            self.tmux_hosts.push(host_id);
        }
    }

    /// tmux 清单成功刷新后清除对应 host 的脏标记。
    pub(super) fn tmux_refreshed(&mut self, host_id: &Option<HostId>) {
        self.tmux_hosts.retain(|host| host != host_id);
    }
}

impl LatestRequestState {
    /// 开始新请求时先取消上一请求，避免迟到的网络结果覆盖用户刚选中的对象。
    pub(super) fn begin(&mut self) -> LatestRequestTicket {
        self.cancel_pending();
        self.request_id = self.request_id.wrapping_add(1).max(1);
        let cancellation = CancellationToken::new();
        self.cancellation = Some(cancellation.clone());
        LatestRequestTicket {
            request_id: self.request_id,
            cancellation,
        }
    }

    /// 只接受当前仍在执行的请求票据，防止关闭、重试或目标切换后的旧结果回写。
    pub(super) fn is_current(&self, ticket: &LatestRequestTicket) -> bool {
        self.request_id == ticket.request_id && self.cancellation.is_some()
    }

    /// 完成当前请求并释放令牌；已经过期的回执不会影响当前操作。
    pub(super) fn complete(&mut self, ticket: &LatestRequestTicket) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        self.cancellation = None;
        true
    }

    /// 目标上下文变化时撤销进行中的请求，且让其票据立即失效。
    pub(super) fn cancel_pending(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
    }
}

impl Default for LatestRequestState {
    /// 尚无用户选择时，详情面板不存在有效请求。
    fn default() -> Self {
        Self {
            request_id: 0,
            cancellation: None,
        }
    }
}

impl Drop for LatestRequestState {
    /// 面板销毁时撤销最后一个详情请求，避免后台结果持有过期目标。
    fn drop(&mut self) {
        self.cancel_pending();
    }
}

impl<T> SectionState<T> {
    /// 创建尚未读取的能力区块；没有数据时 UI 必须展示待加载而非成功空列表。
    pub(super) fn new() -> Self {
        Self {
            data: None,
            phase: SectionPhase::NotLoaded,
            error: None,
            target_generation: 0,
            request_id: 0,
            cancellation: None,
        }
    }

    /// 开始新请求并撤销旧刷新，保留同一目标上次成功数据供明确的 stale 状态呈现。
    pub(super) fn begin(&mut self, target_generation: u64) -> LoadTicket {
        self.cancel_pending();
        self.target_generation = target_generation;
        self.request_id = self.request_id.wrapping_add(1).max(1);
        self.phase = SectionPhase::Loading;
        self.error = None;
        let cancellation = CancellationToken::new();
        self.cancellation = Some(cancellation.clone());
        LoadTicket {
            target_generation,
            request_id: self.request_id,
            cancellation,
        }
    }

    /// 只接收当前目标和当前请求的回执，较晚结束的旧请求不能覆盖新结果。
    pub(super) fn complete(&mut self, ticket: &LoadTicket, result: Result<T, String>) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        self.cancellation = None;
        match result {
            Ok(data) => {
                self.data = Some(data);
                self.phase = SectionPhase::Ready;
                self.error = None;
            }
            Err(error) => {
                self.phase = SectionPhase::Failed;
                self.error = Some(error);
            }
        }
        true
    }

    /// 结束仍当前的取消请求并保留最近成功数据，避免区块永久停留在 loading。
    pub(super) fn cancel(&mut self, ticket: &LoadTicket) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        self.cancellation = None;
        self.phase = SectionPhase::Cancelled;
        self.error = None;
        true
    }

    /// 目标切换时取消未完成读取并清除旧目标数据，避免错误目标被展示成当前状态。
    pub(super) fn reset(&mut self, target_generation: u64) {
        self.cancel_pending();
        self.target_generation = target_generation;
        self.request_id = self.request_id.wrapping_add(1).max(1);
        self.data = None;
        self.phase = SectionPhase::NotLoaded;
        self.error = None;
    }

    /// 返回当前数据快照；失败时数据可能来自上一次成功刷新。
    pub(super) fn data(&self) -> Option<&T> {
        self.data.as_ref()
    }

    /// 返回区块加载阶段，调用方据此分别呈现空、加载、成功或错误状态。
    pub(super) const fn phase(&self) -> SectionPhase {
        self.phase
    }

    /// 返回最后一次读取失败的脱敏展示文本。
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 判断回执是否仍属于最近一次请求，防止切目标后晚到结果污染当前面板。
    pub(super) fn is_current(&self, ticket: &LoadTicket) -> bool {
        self.target_generation == ticket.target_generation
            && self.request_id == ticket.request_id
            && self.phase == SectionPhase::Loading
    }

    /// 取消 section 请求并立刻失效旧票据，避免等待迟到回执时一直显示 Loading。
    pub(super) fn cancel_pending(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
            self.request_id = self.request_id.wrapping_add(1).max(1);
            self.phase = SectionPhase::Cancelled;
            self.error = None;
        }
    }
}

impl<T> Default for SectionState<T> {
    /// 新 section 与显式未读取状态一致，不用默认空集合冒充一次成功查询。
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for SectionState<T> {
    /// 面板离开工作区时撤销未完成的 section 请求，避免目标仍被后台任务持有。
    fn drop(&mut self) {
        self.cancel_pending();
    }
}

/// 将 section 请求同时绑定到页面任务追踪和目标取消令牌，取消时主动通知底层用例。
pub(super) async fn run_section_request<T, F, Fut>(
    executor: ApplicationTaskExecutor,
    request_cancellation: CancellationToken,
    operation: F,
) -> kerminal_application::AppResult<Option<T>>
where
    T: Send + 'static,
    F: FnOnce(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = kerminal_application::AppResult<T>> + Send + 'static,
{
    executor
        .run(move |application_cancellation| {
            run_cancellable_request(application_cancellation, request_cancellation, operation)
        })
        .await
}

/// 将执行器取消与 UI 目标取消合并，保持应用任务边界而不要求 tokio 宏特性。
pub(super) async fn run_cancellable_request<T, F, Fut>(
    application_cancellation: CancellationToken,
    request_cancellation: CancellationToken,
    operation: F,
) -> kerminal_application::AppResult<Option<T>>
where
    T: Send + 'static,
    F: FnOnce(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = kerminal_application::AppResult<T>> + Send + 'static,
{
    let operation_cancellation = application_cancellation.child_token();
    let operation = operation(operation_cancellation.clone());
    match application_cancellation
        .run_until_cancelled(request_cancellation.run_until_cancelled(operation))
        .await
    {
        Some(Some(result)) => result.map(Some),
        _ => {
            operation_cancellation.cancel();
            Ok(None)
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_state.rs"]
mod tests;
