//! 本机、Linux SSH 主机与容器资源采样适配。
//!
//! @author kongweiguang

mod probes;

use std::{
    cmp::Ordering,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CommandEvent, ConnectionIntent, ExecRequest, RemoteConnectionLease,
        RemoteConnectionService, RemoteShellDialect,
    },
    diagnostics::{DiagnosticUnavailableReason, RuntimeGpuHealth},
    operations::{
        CommandExecutionLimits, CommandExecutionTarget, CommandInvocation,
        OperationsCommandExecutor, ServerInfoCollector, ServerInfoTier,
    },
};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ContainerRuntime, CpuSnapshot, DiskSnapshot, GpuSnapshot, MemorySnapshot,
        NetworkCounterSnapshot, NetworkRateSnapshot, ProcessSnapshot, ServerInfoProbeFailure,
        ServerInfoProbeStatus, ServerInfoSampleTimes, ServerInfoSnapshot, ServerInfoTarget,
    },
};
use sysinfo::{
    CpuRefreshKind, Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, System,
};
use tokio_util::sync::CancellationToken;

use self::probes::{failure_status, parse_lspci_gpus, parse_npu_smi_info, parse_nvidia_smi_csv};

const REMOTE_COMMAND_LIMIT: usize = 2 * 1024 * 1024;
const CONTAINER_COMMAND_LIMIT: usize = 256 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(6);
const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const OPTIONAL_DEVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const OPTIONAL_PROBE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const LINUX_SYSTEM_IDENTITY_PROBE: &str = concat!(
    "printf 'os='; uname -s; ",
    "printf '\\narchitecture='; uname -m; ",
    "printf '\\nkernel='; uname -r; ",
    "printf '\\ndistribution='; ",
    "if [ -r /etc/os-release ]; then sed -n 's/^PRETTY_NAME=//p' /etc/os-release | head -n 1; fi; ",
    "printf '\\n'",
);

/// 绑定容器命令所需身份，避免采样 helper 的参数在调用链上分离。
struct ContainerCommandContext<'a> {
    host_id: Option<HostId>,
    runtime_program: &'a str,
    container_id: &'a str,
}

/// 资源采样器复用唯一命令执行器和 SSH 连接服务，本机系统指标由成熟 sysinfo 库采集。
pub struct SystemServerInfoCollector {
    commands: Arc<dyn OperationsCommandExecutor>,
    remote_connections: Arc<dyn RemoteConnectionService>,
}

impl SystemServerInfoCollector {
    /// 显式注入命令和连接端口，便于桌面 composition 与隔离 SSH fixture 共用同一路径。
    pub fn new(
        commands: Arc<dyn OperationsCommandExecutor>,
        remote_connections: Arc<dyn RemoteConnectionService>,
    ) -> Self {
        Self {
            commands,
            remote_connections,
        }
    }
}

#[async_trait]
impl ServerInfoCollector for SystemServerInfoCollector {
    /// 本机采样放入阻塞线程；SSH 复用单个独立 lease；容器采样使用受限 stats 查询。
    async fn collect(
        &self,
        target: ServerInfoTarget,
        tier: ServerInfoTier,
        cancel: CancellationToken,
    ) -> AppResult<ServerInfoSnapshot> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("资源采样已取消"));
        }
        match target {
            ServerInfoTarget::Local => {
                let collect_devices = matches!(tier, ServerInfoTier::Slow | ServerInfoTier::Full);
                let worker = tokio::task::spawn_blocking(move || collect_local_snapshot(tier));
                let mut snapshot = worker.await.map_err(|source| {
                    AppError::infrastructure_with_source("本机资源采样线程失败", source)
                })??;
                if collect_devices {
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled("资源采样已取消"));
                    }
                    let gpu_result = crate::diagnostics::gpu::probe_local_gpus(
                        self.commands.as_ref(),
                        cancel.clone(),
                    )
                    .await;
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled("资源采样已取消"));
                    }
                    snapshot.sample_times.gpu_at_unix_ms = Some(unix_time_ms());
                    match gpu_result {
                        Ok(gpus) => {
                            snapshot.gpus = runtime_gpus_to_snapshots(gpus);
                            snapshot.gpu_probe_status = ServerInfoProbeStatus::Available;
                        }
                        Err(reason) => {
                            snapshot.gpu_probe_status = unavailable_gpu_status(reason);
                        }
                    }
                }
                if cancel.is_cancelled() {
                    return Err(AppError::cancelled("资源采样已取消"));
                }
                Ok(snapshot)
            }
            ServerInfoTarget::Ssh { host_id } => self.collect_remote(host_id, tier, cancel).await,
            ServerInfoTarget::Container {
                host_id,
                runtime,
                container_id,
            } => {
                self.collect_container(host_id, runtime, container_id, tier, cancel)
                    .await
            }
        }
    }
}

impl SystemServerInfoCollector {
    /// 一次 SSH lease 承载同一快照的多个独立 exec channel，读完后统一释放整条跳板路由。
    async fn collect_remote(
        &self,
        host_id: HostId,
        tier: ServerInfoTier,
        cancel: CancellationToken,
    ) -> AppResult<ServerInfoSnapshot> {
        let lease = self
            .remote_connections
            .connect(host_id.clone(), ConnectionIntent::Exec, cancel.clone())
            .await?;
        let sampled =
            collect_linux_snapshot(&lease, ServerInfoTarget::Ssh { host_id }, tier, cancel).await;
        let closed = lease.close("server information sample complete").await;
        match (sampled, closed) {
            (Ok(snapshot), Ok(())) => Ok(snapshot),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(operation), Err(cleanup)) => Err(operation.with_source(cleanup)),
        }
    }

    /// stats 与容器内慢探针分层读取；可选探针失败不会遮蔽其他资源状态。
    ///
    /// @author kongweiguang
    async fn collect_container(
        &self,
        host_id: Option<HostId>,
        runtime: ContainerRuntime,
        container_id: String,
        tier: ServerInfoTier,
        cancel: CancellationToken,
    ) -> AppResult<ServerInfoSnapshot> {
        let snapshot_target = ServerInfoTarget::Container {
            host_id: host_id.clone(),
            runtime,
            container_id: container_id.clone(),
        };
        let runtime_program = match runtime {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        };
        if matches!(tier, ServerInfoTier::Slow | ServerInfoTier::Static) {
            let mut snapshot = empty_snapshot(snapshot_target);
            self.collect_container_details(
                ContainerCommandContext {
                    host_id,
                    runtime_program,
                    container_id: &container_id,
                },
                &mut snapshot,
                tier == ServerInfoTier::Static,
                tier == ServerInfoTier::Slow,
                cancel.clone(),
            )
            .await?;
            if tier == ServerInfoTier::Static
                && snapshot.sample_times.static_info_at_unix_ms.is_none()
            {
                return Err(AppError::unsupported(
                    "容器静态信息采样当前需要返回有效 Linux 系统标识",
                ));
            }
            snapshot.captured_at_unix_ms = unix_time_ms();
            return Ok(snapshot);
        }
        let target = match &host_id {
            Some(host_id) => CommandExecutionTarget::Host(host_id.clone()),
            None => CommandExecutionTarget::Local,
        };
        let result = self
            .commands
            .execute(
                target,
                CommandInvocation::new(
                    runtime_program,
                    vec![
                        "stats".into(),
                        "--no-stream".into(),
                        "--format".into(),
                        "{{json .}}".into(),
                        container_id.clone(),
                    ],
                ),
                CommandExecutionLimits {
                    timeout: COMMAND_TIMEOUT,
                    max_output_bytes: CONTAINER_COMMAND_LIMIT,
                },
                cancel.clone(),
            )
            .await?;
        if result.truncated {
            return Err(AppError::infrastructure("容器资源采样输出超过读取上限"));
        }
        if result.exit_code != Some(0) {
            return Err(AppError::remote_exit(result.exit_code));
        }
        let line = result
            .stdout
            .split(|byte| *byte == b'\n')
            .find(|line| !line.iter().all(u8::is_ascii_whitespace))
            .ok_or_else(|| AppError::not_found("容器资源采样结果为空"))?;
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|_| AppError::infrastructure("容器资源采样结果无法解析"))?;
        let cpu_percent =
            json_string(&value, &["CPUPerc", "cpuPercent"]).and_then(|value| parse_percent(&value));
        let (mut memory_used_bytes, memory_total_bytes) =
            json_string(&value, &["MemUsage", "memoryUsage"])
                .as_deref()
                .and_then(parse_memory_usage)
                .unwrap_or((None, None));
        let memory_percent = json_string(&value, &["MemPerc", "memoryPercent"])
            .and_then(|value| parse_percent(&value));
        if memory_used_bytes.is_none() {
            memory_used_bytes = memory_percent
                .zip(memory_total_bytes)
                .map(|(percent, total)| (total as f64 * percent / 100.0) as u64);
        }
        let memory = MemorySnapshot {
            total_bytes: memory_total_bytes,
            used_bytes: memory_used_bytes,
            available_bytes: memory_total_bytes
                .zip(memory_used_bytes)
                .map(|(total, used)| total.saturating_sub(used)),
            ..MemorySnapshot::default()
        };
        let process_count =
            json_string(&value, &["PIDs", "Pids", "pids"]).and_then(|value| value.parse().ok());
        let network_counters = json_string(&value, &["NetIO", "netIO", "networkIO"])
            .as_deref()
            .and_then(parse_network_io)
            .map(|(rx_bytes, tx_bytes)| {
                vec![NetworkCounterSnapshot {
                    name: "container".to_owned(),
                    rx_bytes,
                    tx_bytes,
                }]
            });
        let captured_at = unix_time_ms();
        let mut snapshot = ServerInfoSnapshot {
            target: snapshot_target,
            captured_at_unix_ms: captured_at,
            sample_times: ServerInfoSampleTimes {
                fast_at_unix_ms: Some(captured_at),
                ..ServerInfoSampleTimes::default()
            },
            uptime_seconds: None,
            hostname: None,
            operating_system: None,
            architecture: None,
            kernel: None,
            cpu: CpuSnapshot {
                usage_percent: cpu_percent,
                ..CpuSnapshot::default()
            },
            memory,
            disks: Vec::new(),
            network_counters: network_counters.clone().unwrap_or_default(),
            network_rates: network_counters
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|counter| NetworkRateSnapshot {
                    name: counter.name.clone(),
                    rx_bytes_per_second: None,
                    tx_bytes_per_second: None,
                    counter_reset: false,
                })
                .collect(),
            process_count,
            running_process_count: None,
            processes: Vec::new(),
            disk_probe_status: ServerInfoProbeStatus::NotSampled,
            network_probe_status: network_counters.map_or(
                ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::InvalidData),
                |_| ServerInfoProbeStatus::Available,
            ),
            process_probe_status: process_count.map_or(
                ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::InvalidData),
                |_| ServerInfoProbeStatus::Available,
            ),
            gpu_probe_status: ServerInfoProbeStatus::NotSampled,
            gpus: Vec::new(),
            npu_probe_status: ServerInfoProbeStatus::NotSampled,
            npus: Vec::new(),
        };
        if tier == ServerInfoTier::Full {
            self.collect_container_details(
                ContainerCommandContext {
                    host_id,
                    runtime_program,
                    container_id: &container_id,
                },
                &mut snapshot,
                true,
                true,
                cancel,
            )
            .await?;
        }
        Ok(snapshot)
    }

    /// 通过 runtime exec 按目标层级补采容器内部字段，命令缺失只影响对应 probe 状态。
    ///
    /// @author kongweiguang
    async fn collect_container_details(
        &self,
        command_context: ContainerCommandContext<'_>,
        snapshot: &mut ServerInfoSnapshot,
        collect_static: bool,
        collect_slow: bool,
        cancel: CancellationToken,
    ) -> AppResult<()> {
        let ContainerCommandContext {
            host_id,
            runtime_program,
            container_id,
        } = command_context;
        if collect_static {
            let (identity_output, _) = self
                .probe_container_command(
                    host_id.clone(),
                    runtime_program,
                    container_id,
                    "sh",
                    vec!["-c".into(), LINUX_SYSTEM_IDENTITY_PROBE.into()],
                    cancel.clone(),
                )
                .await?;
            if let Some(output) = identity_output {
                let (platform, distribution, architecture, kernel) =
                    parse_linux_identity_output(&output);
                if platform
                    .as_deref()
                    .is_some_and(|platform| platform.eq_ignore_ascii_case("linux"))
                {
                    snapshot.operating_system = distribution.or(platform);
                    snapshot.architecture = architecture;
                    snapshot.kernel = kernel;
                    snapshot.sample_times.static_info_at_unix_ms = Some(unix_time_ms());
                }
            }
            let (hostname_output, _) = self
                .probe_container_command(
                    host_id.clone(),
                    runtime_program,
                    container_id,
                    "hostname",
                    Vec::new(),
                    cancel.clone(),
                )
                .await?;
            snapshot.hostname = hostname_output.and_then(nonempty);
        }
        if !collect_slow {
            return Ok(());
        }

        let (disk_output, disk_status) = self
            .probe_container_command(
                host_id.clone(),
                runtime_program,
                container_id,
                "df",
                vec!["-Pk".into()],
                cancel.clone(),
            )
            .await?;
        snapshot.disks = disk_output
            .as_deref()
            .map(parse_linux_disks)
            .unwrap_or_default();
        snapshot.disk_probe_status =
            if snapshot.disks.is_empty() && disk_status == ServerInfoProbeStatus::Available {
                ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::InvalidData)
            } else {
                disk_status
            };

        let (network_output, network_status) = self
            .probe_container_command(
                host_id.clone(),
                runtime_program,
                container_id,
                "cat",
                vec!["/proc/net/dev".into()],
                cancel.clone(),
            )
            .await?;
        if let Some(output) = network_output {
            let counters = parse_linux_network(&output);
            if !counters.is_empty() {
                snapshot.network_counters = counters;
                snapshot.network_rates = snapshot
                    .network_counters
                    .iter()
                    .map(|counter| NetworkRateSnapshot {
                        name: counter.name.clone(),
                        rx_bytes_per_second: None,
                        tx_bytes_per_second: None,
                        counter_reset: false,
                    })
                    .collect();
                snapshot.network_probe_status = ServerInfoProbeStatus::Available;
            } else if snapshot.network_counters.is_empty() {
                snapshot.network_probe_status =
                    ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::InvalidData);
            }
        } else if snapshot.network_counters.is_empty() {
            snapshot.network_probe_status = network_status;
        }

        let (process_output, process_status) = self
            .probe_container_command(
                host_id.clone(),
                runtime_program,
                container_id,
                "ps",
                vec!["-eo".into(), "pid=,stat=,%cpu=,%mem=,rss=,comm=".into()],
                cancel.clone(),
            )
            .await?;
        if let Some(output) = process_output {
            let (count, running, processes) = parse_linux_processes(&output);
            snapshot.process_count = count;
            snapshot.running_process_count = running;
            snapshot.processes = processes;
            snapshot.process_probe_status = count.map_or(
                ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::InvalidData),
                |_| ServerInfoProbeStatus::Available,
            );
        } else {
            snapshot.process_probe_status = process_status;
        }

        let (gpu_output, gpu_status) = self
            .probe_container_command(
                host_id.clone(),
                runtime_program,
                container_id,
                "nvidia-smi",
                vec![
                    "--query-gpu=name,driver_version,memory.total,memory.used,utilization.gpu,temperature.gpu,power.draw".into(),
                    "--format=csv,noheader,nounits".into(),
                ],
                cancel.clone(),
            )
            .await?;
        let (mut gpus, mut resolved_gpu_status) = match gpu_output {
            Some(output) => match parse_nvidia_smi_csv(&output) {
                Ok(gpus) if !gpus.is_empty() => (gpus, ServerInfoProbeStatus::Available),
                Ok(_) => (Vec::new(), gpu_status),
                Err(failure) => (Vec::new(), ServerInfoProbeStatus::Unavailable(failure)),
            },
            None => (Vec::new(), gpu_status),
        };
        if gpus.is_empty() {
            let (lspci_output, lspci_status) = self
                .probe_container_command(
                    host_id.clone(),
                    runtime_program,
                    container_id,
                    "lspci",
                    Vec::new(),
                    cancel.clone(),
                )
                .await?;
            if let Some(output) = lspci_output {
                gpus = parse_lspci_gpus(&output);
                resolved_gpu_status = ServerInfoProbeStatus::Available;
            } else if !matches!(
                resolved_gpu_status,
                ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::PermissionDenied)
                    | ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::TimedOut)
                    | ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::CommandFailed)
            ) {
                resolved_gpu_status = lspci_status;
            }
        }
        snapshot.gpus = gpus;
        snapshot.gpu_probe_status = resolved_gpu_status;

        let (npu_output, npu_status) = self
            .probe_container_command(
                host_id,
                runtime_program,
                container_id,
                "npu-smi",
                vec!["info".into()],
                cancel,
            )
            .await?;
        match npu_output {
            Some(output) => match parse_npu_smi_info(&output) {
                Ok(npus) => {
                    snapshot.npus = npus;
                    snapshot.npu_probe_status = ServerInfoProbeStatus::Available;
                }
                Err(failure) => {
                    snapshot.npu_probe_status = ServerInfoProbeStatus::Unavailable(failure);
                }
            },
            None => snapshot.npu_probe_status = npu_status,
        }
        let sampled_at = unix_time_ms();
        snapshot.sample_times.slow_at_unix_ms = Some(sampled_at);
        snapshot.sample_times.gpu_at_unix_ms = Some(sampled_at);
        snapshot.sample_times.npu_at_unix_ms = Some(sampled_at);
        Ok(())
    }

    /// 容器探针用固定 argv 调用 runtime exec，避免 shell 拼接容器名或内部路径。
    async fn execute_container_command(
        &self,
        host_id: Option<HostId>,
        runtime_program: &str,
        container_id: &str,
        program: &str,
        arguments: Vec<String>,
        cancel: CancellationToken,
    ) -> AppResult<String> {
        let target = match host_id {
            Some(host_id) => CommandExecutionTarget::Host(host_id),
            None => CommandExecutionTarget::Local,
        };
        let mut args = vec![
            "exec".into(),
            "--".into(),
            container_id.to_owned(),
            program.to_owned(),
        ];
        args.extend(arguments);
        let result = self
            .commands
            .execute(
                target,
                CommandInvocation::new(runtime_program, args),
                CommandExecutionLimits {
                    timeout: COMMAND_TIMEOUT,
                    max_output_bytes: CONTAINER_COMMAND_LIMIT,
                },
                cancel,
            )
            .await?;
        if result.truncated {
            return Err(AppError::infrastructure("容器资源采样输出超过读取上限"));
        }
        if result.exit_code != Some(0) {
            return Err(AppError::remote_exit(result.exit_code));
        }
        String::from_utf8(result.stdout)
            .map_err(|_| AppError::infrastructure("容器资源采样结果不是 UTF-8"))
    }

    /// 可选容器命令失败时只投影脱敏状态，取消则仍终止当前快照请求。
    async fn probe_container_command(
        &self,
        host_id: Option<HostId>,
        runtime_program: &str,
        container_id: &str,
        program: &str,
        arguments: Vec<String>,
        cancel: CancellationToken,
    ) -> AppResult<(Option<String>, ServerInfoProbeStatus)> {
        match self
            .execute_container_command(
                host_id,
                runtime_program,
                container_id,
                program,
                arguments,
                cancel.clone(),
            )
            .await
        {
            Ok(output) => Ok((Some(output), ServerInfoProbeStatus::Available)),
            Err(error) if cancel.is_cancelled() => Err(error),
            Err(error) => Ok((None, failure_status(&error))),
        }
    }
}

/// sysinfo 提供跨平台系统指标；CPU 用两个样本取差，避免首帧 0% 被误认为真实空闲。
/// sysinfo 未提供与 Linux `Cached` 同语义的可靠值，因此保留 None 而不从容量差值猜测。
///
/// @author kongweiguang
fn collect_local_snapshot(tier: ServerInfoTier) -> AppResult<ServerInfoSnapshot> {
    let collect_fast = matches!(tier, ServerInfoTier::Fast | ServerInfoTier::Full);
    let collect_slow = matches!(tier, ServerInfoTier::Slow | ServerInfoTier::Full);
    let collect_static = matches!(tier, ServerInfoTier::Static | ServerInfoTier::Full);
    let cpu_refresh = CpuRefreshKind::new().with_cpu_usage();
    let process_refresh = ProcessRefreshKind::new().with_cpu().with_memory();
    let mut refresh = RefreshKind::new();
    if collect_fast || collect_static {
        refresh = refresh.with_cpu(if collect_fast {
            cpu_refresh
        } else {
            CpuRefreshKind::new()
        });
    }
    if collect_fast || collect_slow {
        refresh = refresh.with_memory(MemoryRefreshKind::everything());
    }
    if collect_slow {
        refresh = refresh.with_processes(process_refresh);
    }
    let mut system = System::new_with_specifics(refresh);
    if collect_slow {
        system.refresh_processes_specifics(ProcessesToUpdate::All, process_refresh);
    }
    if collect_fast {
        std::thread::sleep(CPU_SAMPLE_INTERVAL);
        system.refresh_cpu_specifics(cpu_refresh);
        if collect_slow {
            system.refresh_processes_specifics(ProcessesToUpdate::All, process_refresh);
        }
    }
    let load = System::load_average();
    let total_memory = system.total_memory();
    let used_memory = system.used_memory();
    let total_swap = system.total_swap();
    let used_swap = system.used_swap();
    let cpu = CpuSnapshot {
        usage_percent: collect_fast
            .then(|| finite_percent(system.global_cpu_usage()))
            .flatten(),
        per_core_percent: if collect_fast {
            system
                .cpus()
                .iter()
                .map(|cpu| finite_percent(cpu.cpu_usage()))
                .collect()
        } else {
            Vec::new()
        },
        load_average: if collect_fast {
            [load.one, load.five, load.fifteen]
                .into_iter()
                .filter(|value| value.is_finite())
                .collect()
        } else {
            Vec::new()
        },
        model: collect_static
            .then(|| {
                system
                    .cpus()
                    .first()
                    .map(|cpu| cpu.brand().trim().to_owned())
                    .filter(|model| !model.is_empty())
            })
            .flatten(),
    };
    let memory = if collect_fast {
        MemorySnapshot {
            total_bytes: positive(total_memory),
            used_bytes: positive(used_memory),
            available_bytes: positive(system.available_memory()),
            // sysinfo 未提供可靠的 page-cache 值时保持未知，不用 total-used 估算 Cached。
            cached_bytes: None,
            swap_total_bytes: positive(total_swap),
            swap_used_bytes: positive(used_swap),
        }
    } else {
        MemorySnapshot::default()
    };
    let disks: Vec<_> = if collect_slow {
        Disks::new_with_refreshed_list()
            .list()
            .iter()
            .map(|disk| {
                let total = disk.total_space();
                let available = disk.available_space();
                DiskSnapshot {
                    filesystem: disk.file_system().to_string_lossy().into_owned(),
                    mount: disk.mount_point().to_string_lossy().into_owned(),
                    total_bytes: positive(total),
                    used_bytes: total.checked_sub(available),
                    available_bytes: Some(available),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let network_counters: Vec<_> = if collect_fast {
        Networks::new_with_refreshed_list()
            .list()
            .iter()
            .map(|(name, network)| NetworkCounterSnapshot {
                name: name.clone(),
                rx_bytes: network.total_received(),
                tx_bytes: network.total_transmitted(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let network_rates = network_counters
        .iter()
        .map(|counter| NetworkRateSnapshot {
            name: counter.name.clone(),
            rx_bytes_per_second: None,
            tx_bytes_per_second: None,
            counter_reset: false,
        })
        .collect();
    let processes: Vec<_> = if collect_slow {
        system
            .processes()
            .values()
            .map(|process| ProcessSnapshot {
                pid: process.pid().as_u32(),
                name: process.name().to_string_lossy().into_owned(),
                cpu_percent: collect_fast
                    .then(|| finite_percent(process.cpu_usage()))
                    .flatten(),
                memory_percent: (total_memory > 0)
                    .then(|| process.memory() as f64 * 100.0 / total_memory as f64),
                memory_bytes: positive(process.memory()),
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut processes = processes;
    processes.sort_by(|left, right| {
        right
            .cpu_percent
            .partial_cmp(&left.cpu_percent)
            .unwrap_or(Ordering::Equal)
    });
    let process_count = collect_slow.then(|| system.processes().len() as u64);
    let running_process_count = collect_slow.then(|| {
        system
            .processes()
            .values()
            .filter(|process| process.status() == sysinfo::ProcessStatus::Run)
            .count() as u64
    });
    processes.truncate(100);
    let captured_at = unix_time_ms();
    Ok(ServerInfoSnapshot {
        target: ServerInfoTarget::Local,
        captured_at_unix_ms: captured_at,
        sample_times: ServerInfoSampleTimes {
            static_info_at_unix_ms: collect_static.then_some(captured_at),
            fast_at_unix_ms: collect_fast.then_some(captured_at),
            slow_at_unix_ms: collect_slow.then_some(captured_at),
            ..ServerInfoSampleTimes::default()
        },
        uptime_seconds: collect_fast.then(System::uptime),
        hostname: collect_static.then(System::host_name).flatten(),
        operating_system: collect_static
            .then(|| System::long_os_version().or_else(System::name))
            .flatten(),
        architecture: collect_static.then(|| std::env::consts::ARCH.to_owned()),
        kernel: collect_static.then(System::kernel_version).flatten(),
        cpu,
        memory,
        disks,
        network_counters,
        network_rates,
        process_count,
        running_process_count,
        processes,
        disk_probe_status: probe_status_for_tier(collect_slow),
        network_probe_status: probe_status_for_tier(collect_fast),
        process_probe_status: probe_status_for_tier(collect_slow),
        gpu_probe_status: ServerInfoProbeStatus::NotSampled,
        gpus: Vec::new(),
        npu_probe_status: ServerInfoProbeStatus::NotSampled,
        npus: Vec::new(),
    })
}

/// 未执行的分区明确保留 NotSampled，避免空列表被误认为设备不存在。
fn probe_status_for_tier(sampled: bool) -> ServerInfoProbeStatus {
    if sampled {
        ServerInfoProbeStatus::Available
    } else {
        ServerInfoProbeStatus::NotSampled
    }
}

/// 把诊断 GPU 契约投影到运维快照，保留平台共有字段且不伪造功耗数据。
fn runtime_gpus_to_snapshots(gpus: Vec<RuntimeGpuHealth>) -> Vec<GpuSnapshot> {
    gpus.into_iter()
        .enumerate()
        .map(|(index, gpu)| GpuSnapshot {
            id: index.to_string(),
            model: Some(gpu.name),
            vendor: gpu.vendor,
            driver_version: gpu.driver_version,
            utilization_percent: gpu.utilization_percent.map(f64::from),
            memory_total_bytes: gpu.memory_total_bytes,
            memory_used_bytes: gpu.memory_used_bytes,
            temperature_celsius: gpu.temperature_celsius.map(f64::from),
            power_watts: None,
        })
        .collect()
}

/// 将诊断探针错误压缩成 UI 可解释原因，不暴露本机命令、路径或底层错误文本。
fn unavailable_gpu_status(
    reason: crate::diagnostics::gpu::LocalGpuProbeFailure,
) -> ServerInfoProbeStatus {
    let failure = match reason {
        crate::diagnostics::gpu::LocalGpuProbeFailure::TimedOut => ServerInfoProbeFailure::TimedOut,
        crate::diagnostics::gpu::LocalGpuProbeFailure::Unavailable(reason) => match reason {
            DiagnosticUnavailableReason::NotFound => ServerInfoProbeFailure::NotFound,
            DiagnosticUnavailableReason::InvalidData => ServerInfoProbeFailure::InvalidData,
            DiagnosticUnavailableReason::PermissionDenied => {
                ServerInfoProbeFailure::PermissionDenied
            }
            DiagnosticUnavailableReason::Unsupported => ServerInfoProbeFailure::Unsupported,
            DiagnosticUnavailableReason::CollectionFailed => ServerInfoProbeFailure::CommandFailed,
        },
    };
    ServerInfoProbeStatus::Unavailable(failure)
}

/// 非监控层级返回带显式目标的空投影，不发出无关容器 runtime 命令。
///
/// @author kongweiguang
fn empty_snapshot(target: ServerInfoTarget) -> ServerInfoSnapshot {
    ServerInfoSnapshot {
        target,
        captured_at_unix_ms: unix_time_ms(),
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

/// Linux 静态身份通过单次固定 sh 脚本输出三个带标签字段，避免依赖 uname 多 flag 的平台输出顺序。
///
/// @author kongweiguang
async fn collect_linux_snapshot(
    lease: &RemoteConnectionLease,
    target: ServerInfoTarget,
    tier: ServerInfoTier,
    cancel: CancellationToken,
) -> AppResult<ServerInfoSnapshot> {
    if !matches!(
        lease.shell_dialect(),
        RemoteShellDialect::Posix | RemoteShellDialect::Fish
    ) {
        return Err(AppError::unsupported(
            "远程资源采样需要已识别的 POSIX 或 Fish shell",
        ));
    }
    let collect_fast = matches!(tier, ServerInfoTier::Fast | ServerInfoTier::Full);
    let collect_slow = matches!(tier, ServerInfoTier::Slow | ServerInfoTier::Full);
    let collect_static = matches!(tier, ServerInfoTier::Static | ServerInfoTier::Full);
    let (operating_system, architecture, kernel, hostname, cpuinfo) = if collect_static {
        let platform = run_remote(
            lease,
            "sh",
            vec!["-c".into(), LINUX_SYSTEM_IDENTITY_PROBE.into()],
            &cancel,
        )
        .await?;
        let (platform, distribution, architecture, kernel) = parse_linux_identity_output(&platform);
        let platform = platform.ok_or_else(|| AppError::unsupported("无法识别远程 Linux 主机"))?;
        if !platform.eq_ignore_ascii_case("linux") {
            return Err(AppError::unsupported("远程资源采样当前仅实现 Linux 主机"));
        }
        let operating_system = distribution.unwrap_or(platform);
        let hostname = nonempty(run_remote(lease, "hostname", Vec::new(), &cancel).await?);
        let cpuinfo = run_remote(lease, "cat", vec!["/proc/cpuinfo".into()], &cancel).await?;
        (operating_system, architecture, kernel, hostname, cpuinfo)
    } else {
        let operating_system = run_remote(lease, "uname", vec!["-s".into()], &cancel).await?;
        if !operating_system.trim().eq_ignore_ascii_case("linux") {
            return Err(AppError::unsupported("远程资源采样当前仅实现 Linux 主机"));
        }
        (
            operating_system.trim().to_owned(),
            None,
            None,
            None,
            String::new(),
        )
    };
    let (mut cpu, memory, network_counters, uptime_seconds) = if collect_fast {
        // Uptime 是可选附加信息；单文件读取失败不应丢弃 CPU/内存/网络快照，但取消仍须终止整次采样。
        let uptime_text = match run_remote(lease, "cat", vec!["/proc/uptime".into()], &cancel).await
        {
            Ok(output) => Some(output),
            Err(error) if cancel.is_cancelled() => return Err(error),
            Err(_) => None,
        };
        let uptime_seconds = uptime_text.as_deref().and_then(parse_linux_uptime);
        let load_text = run_remote(lease, "cat", vec!["/proc/loadavg".into()], &cancel).await?;
        let mem_text = run_remote(lease, "cat", vec!["/proc/meminfo".into()], &cancel).await?;
        let first_cpu = run_remote(lease, "cat", vec!["/proc/stat".into()], &cancel).await?;
        tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("资源采样已取消")),
            _ = tokio::time::sleep(CPU_SAMPLE_INTERVAL) => {}
        }
        let second_cpu = run_remote(lease, "cat", vec!["/proc/stat".into()], &cancel).await?;
        let cpu = parse_linux_cpu(&first_cpu, &second_cpu, &load_text, &cpuinfo)?;
        let memory = parse_linux_memory(&mem_text)?;
        let network_text = run_remote(lease, "cat", vec!["/proc/net/dev".into()], &cancel).await?;
        (
            cpu,
            memory,
            parse_linux_network(&network_text),
            uptime_seconds,
        )
    } else {
        (
            CpuSnapshot::default(),
            MemorySnapshot::default(),
            Vec::new(),
            None,
        )
    };
    if collect_static && cpu.model.is_none() {
        cpu.model = cpuinfo
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| matches!(key.trim(), "model name" | "Hardware"))
            })
            .map(|(_, value)| value.trim().to_owned())
            .filter(|model| !model.is_empty());
    }
    let disks = if collect_slow {
        parse_linux_disks(&run_remote(lease, "df", vec!["-Pk".into()], &cancel).await?)
    } else {
        Vec::new()
    };
    let (process_count, running_process_count, processes) = if collect_slow {
        let processes_text = run_remote(
            lease,
            "ps",
            vec!["-eo".into(), "pid=,stat=,%cpu=,%mem=,rss=,comm=".into()],
            &cancel,
        )
        .await?;
        parse_linux_processes(&processes_text)
    } else {
        (None, None, Vec::new())
    };
    let (gpus, gpu_probe_status, npus, npu_probe_status) = if collect_slow {
        let (gpu_result, npu_result) = tokio::join!(
            collect_linux_gpus(lease, &cancel),
            collect_linux_npus(lease, &cancel),
        );
        let (gpus, gpu_probe_status) = gpu_result?;
        let (npus, npu_probe_status) = npu_result?;
        (gpus, gpu_probe_status, npus, npu_probe_status)
    } else {
        (
            Vec::new(),
            ServerInfoProbeStatus::NotSampled,
            Vec::new(),
            ServerInfoProbeStatus::NotSampled,
        )
    };
    let captured_at = unix_time_ms();
    Ok(ServerInfoSnapshot {
        target,
        captured_at_unix_ms: captured_at,
        sample_times: ServerInfoSampleTimes {
            static_info_at_unix_ms: collect_static.then_some(captured_at),
            fast_at_unix_ms: collect_fast.then_some(captured_at),
            slow_at_unix_ms: collect_slow.then_some(captured_at),
            gpu_at_unix_ms: collect_slow.then_some(captured_at),
            npu_at_unix_ms: collect_slow.then_some(captured_at),
        },
        uptime_seconds,
        hostname,
        operating_system: collect_static.then(|| operating_system.trim().to_owned()),
        architecture,
        kernel,
        cpu,
        memory,
        disks,
        network_rates: network_counters
            .iter()
            .map(|counter| NetworkRateSnapshot {
                name: counter.name.clone(),
                rx_bytes_per_second: None,
                tx_bytes_per_second: None,
                counter_reset: false,
            })
            .collect(),
        network_counters,
        process_count,
        running_process_count,
        processes,
        disk_probe_status: probe_status_for_tier(collect_slow),
        network_probe_status: probe_status_for_tier(collect_fast),
        process_probe_status: probe_status_for_tier(collect_slow),
        gpu_probe_status,
        gpus,
        npu_probe_status,
        npus,
    })
}

/// 保留 uname 的 Linux 平台校验，并单独解析发行版展示名，缺失时由调用方回退到 Linux。
///
/// @author kongweiguang
fn parse_linux_identity_output(
    output: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let mut platform = None;
    let mut architecture = None;
    let mut kernel = None;
    let mut distribution = None;
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = nonempty(value.to_owned());
        match key {
            "os" => platform = value,
            "architecture" => architecture = value,
            "kernel" => kernel = value,
            "distribution" => {
                distribution = value
                    .map(|value| {
                        let value = value.trim();
                        value
                            .strip_prefix('"')
                            .and_then(|value| value.strip_suffix('"'))
                            .or_else(|| {
                                value
                                    .strip_prefix('\'')
                                    .and_then(|value| value.strip_suffix('\''))
                            })
                            .unwrap_or(value)
                            .to_owned()
                    })
                    .and_then(nonempty)
            }
            _ => {}
        }
    }
    (platform, distribution, architecture, kernel)
}

/// Linux `/proc/uptime` 首列允许小数秒；拒绝负数、非有限值和损坏文本，避免把未知伪装成零。
///
/// @author kongweiguang
fn parse_linux_uptime(output: &str) -> Option<u64> {
    let seconds = output.split_whitespace().next()?.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| seconds.floor() as u64)
}

/// SSH GPU 探针优先读取 nvidia-smi 明细，回退 lspci 只识别设备、不伪造计量。
async fn collect_linux_gpus(
    lease: &RemoteConnectionLease,
    cancel: &CancellationToken,
) -> AppResult<(
    Vec<kerminal_domain::operations::GpuSnapshot>,
    ServerInfoProbeStatus,
)> {
    let (nvidia_output, nvidia_status) = run_remote_optional(
        lease,
        "nvidia-smi",
        vec![
            "--query-gpu=name,driver_version,memory.total,memory.used,utilization.gpu,temperature.gpu,power.draw".into(),
            "--format=csv,noheader,nounits".into(),
        ],
        cancel,
    )
    .await?;
    if let Some(output) = nvidia_output {
        match parse_nvidia_smi_csv(&output) {
            Ok(gpus) if !gpus.is_empty() => {
                return Ok((gpus, ServerInfoProbeStatus::Available));
            }
            Ok(_) => {}
            Err(failure) => {
                return Ok((Vec::new(), ServerInfoProbeStatus::Unavailable(failure)));
            }
        }
    }

    let (lspci_output, lspci_status) =
        run_remote_optional(lease, "lspci", Vec::new(), cancel).await?;
    if let Some(output) = lspci_output {
        return Ok((parse_lspci_gpus(&output), ServerInfoProbeStatus::Available));
    }
    let status = if matches!(
        nvidia_status,
        ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::PermissionDenied)
            | ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::TimedOut)
            | ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::CommandFailed)
    ) {
        nvidia_status
    } else {
        lspci_status
    };
    Ok((Vec::new(), status))
}

/// SSH NPU 探针只解析成熟 npu-smi 的公开文本结果，缺少命令时保留明确状态。
async fn collect_linux_npus(
    lease: &RemoteConnectionLease,
    cancel: &CancellationToken,
) -> AppResult<(
    Vec<kerminal_domain::operations::NpuSnapshot>,
    ServerInfoProbeStatus,
)> {
    let (output, status) =
        run_remote_optional(lease, "npu-smi", vec!["info".into()], cancel).await?;
    let Some(output) = output else {
        return Ok((Vec::new(), status));
    };
    match parse_npu_smi_info(&output) {
        Ok(npus) => Ok((npus, ServerInfoProbeStatus::Available)),
        Err(failure) => Ok((Vec::new(), ServerInfoProbeStatus::Unavailable(failure))),
    }
}

/// 对可选硬件命令使用有限超时；超时或取消时先撤销 exec stream，再交付稳定分类。
async fn run_remote_optional(
    lease: &RemoteConnectionLease,
    program: &str,
    args: Vec<String>,
    cancel: &CancellationToken,
) -> AppResult<(Option<String>, ServerInfoProbeStatus)> {
    let operation_cancel = cancel.child_token();
    let operation = run_remote(lease, program, args, &operation_cancel);
    tokio::pin!(operation);
    let result = tokio::select! {
        result = &mut operation => return optional_probe_result(result, cancel),
        _ = cancel.cancelled() => {
            operation_cancel.cancel();
            let _ = tokio::time::timeout(OPTIONAL_PROBE_CLEANUP_TIMEOUT, &mut operation).await;
            return Err(AppError::cancelled("硬件资源采样已取消"));
        }
        _ = tokio::time::sleep(OPTIONAL_DEVICE_PROBE_TIMEOUT) => {
            operation_cancel.cancel();
            let _ = tokio::time::timeout(OPTIONAL_PROBE_CLEANUP_TIMEOUT, &mut operation).await;
            Err(AppError::timeout("可选硬件资源探针"))
        }
    };
    optional_probe_result(result, cancel)
}

/// 可选探针失败只转换为脱敏状态；用户取消仍向上返回并停止整个采样请求。
fn optional_probe_result(
    result: AppResult<String>,
    cancel: &CancellationToken,
) -> AppResult<(Option<String>, ServerInfoProbeStatus)> {
    if cancel.is_cancelled() {
        return Err(AppError::cancelled("硬件资源采样已取消"));
    }
    match result {
        Ok(output) => Ok((Some(output), ServerInfoProbeStatus::Available)),
        Err(error) if error.public_code() == kerminal_application::PublicErrorCode::Cancelled => {
            Err(error)
        }
        Err(error) => Ok((None, failure_status(&error))),
    }
}

/// Drain 已经退出的 SSH exec 到 EOF，并保留 stderr/非零退出，不能丢尾部输出。
async fn run_remote(
    lease: &RemoteConnectionLease,
    program: &str,
    args: Vec<String>,
    cancel: &CancellationToken,
) -> AppResult<String> {
    let mut stream = lease
        .exec_stream(
            ExecRequest {
                program: program.to_owned(),
                args,
                environment: Default::default(),
                working_directory: None,
            },
            cancel.child_token(),
        )
        .await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    let operation = async {
        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled("资源采样已取消")),
                event = stream.next_event() => event?,
            };
            let Some(event) = event else { break };
            match event {
                CommandEvent::Stdout(bytes) => append_limited(&mut stdout, bytes)?,
                CommandEvent::Stderr(bytes) => append_limited(&mut stderr, bytes)?,
                CommandEvent::Exited { code, signal } => {
                    if signal.is_some() {
                        return Err(AppError::infrastructure("远程资源探针被信号中断"));
                    }
                    exit_code = code.and_then(|code| i32::try_from(code).ok());
                }
            }
        }
        if exit_code != Some(0) {
            return Err(AppError::remote_exit(exit_code));
        }
        String::from_utf8(stdout)
            .map_err(|_| AppError::infrastructure("远程资源探针输出不是 UTF-8"))
    }
    .await;
    let closed = stream.close().await;
    match (operation, closed) {
        (Ok(output), Ok(())) => Ok(output),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(operation), Err(cleanup)) => Err(operation.with_source(cleanup)),
    }
}

/// 单条命令输出设有上限，资源列表过大时返回错误而不是解析截断内容。
fn append_limited(output: &mut Vec<u8>, bytes: Vec<u8>) -> AppResult<()> {
    if output.len().saturating_add(bytes.len()) > REMOTE_COMMAND_LIMIT {
        return Err(AppError::infrastructure("远程资源探针输出超过读取上限"));
    }
    output.extend_from_slice(&bytes);
    Ok(())
}

/// 基于两帧 `/proc/stat` 计算瞬时 CPU 使用率；计数无效时不伪造零值。
fn parse_linux_cpu(before: &str, after: &str, load: &str, cpuinfo: &str) -> AppResult<CpuSnapshot> {
    let previous = parse_cpu_counters(before)?;
    let current = parse_cpu_counters(after)?;
    let usage = cpu_usage(&previous[0], &current[0]);
    let load_average = load
        .split_whitespace()
        .take(3)
        .filter_map(|part| part.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .collect();
    let model = cpuinfo
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| matches!(key.trim(), "model name" | "Hardware"))
        })
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    Ok(CpuSnapshot {
        usage_percent: usage,
        per_core_percent: current
            .iter()
            .zip(previous.iter())
            .skip(1)
            .map(|(now, then)| cpu_usage(then, now))
            .collect(),
        load_average,
        model,
    })
}

/// 解析 aggregate 与 per-core CPU tick；第一项必须为 aggregate cpu 行。
fn parse_cpu_counters(output: &str) -> AppResult<Vec<Vec<u64>>> {
    let counters: Vec<_> = output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let label = fields.next()?;
            if label != "cpu"
                && !label
                    .strip_prefix("cpu")
                    .is_some_and(|suffix| suffix.parse::<u32>().is_ok())
            {
                return None;
            }
            let values: Vec<_> = fields.filter_map(|field| field.parse().ok()).collect();
            (!values.is_empty()).then_some(values)
        })
        .collect();
    if counters.is_empty() || counters[0].len() < 4 {
        return Err(AppError::infrastructure("远程 CPU 计数格式无效"));
    }
    Ok(counters)
}

/// 以 busy/total tick 差计算百分比，防止回绕和零分母产生 NaN。
fn cpu_usage(previous: &[u64], current: &[u64]) -> Option<f64> {
    let previous_total: u64 = previous.iter().copied().sum();
    let current_total: u64 = current.iter().copied().sum();
    if current_total <= previous_total || previous.len() < 4 || current.len() < 4 {
        return None;
    }
    let previous_idle = previous[3].saturating_add(previous.get(4).copied().unwrap_or(0));
    let current_idle = current[3].saturating_add(current.get(4).copied().unwrap_or(0));
    let elapsed = current_total - previous_total;
    let idle = current_idle.saturating_sub(previous_idle).min(elapsed);
    Some((elapsed - idle) as f64 * 100.0 / elapsed as f64)
}

/// 将 Linux meminfo 的 KiB 单位统一换算为字节；Cached 是可选观测，缺失时不推算。
///
/// @author kongweiguang
fn parse_linux_memory(output: &str) -> AppResult<MemorySnapshot> {
    let values: std::collections::HashMap<_, _> = output
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let value = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((name, value.saturating_mul(1024)))
        })
        .collect();
    let total = values.get("MemTotal").copied();
    let available = values
        .get("MemAvailable")
        .copied()
        .or_else(|| values.get("MemFree").copied());
    if total.is_none() || available.is_none() {
        return Err(AppError::infrastructure("远程内存采样字段缺失"));
    }
    let swap_total = values.get("SwapTotal").copied();
    let swap_free = values.get("SwapFree").copied();
    Ok(MemorySnapshot {
        total_bytes: total,
        used_bytes: total
            .zip(available)
            .map(|(total, available)| total.saturating_sub(available)),
        available_bytes: available,
        cached_bytes: values.get("Cached").copied(),
        swap_total_bytes: swap_total,
        swap_used_bytes: swap_total
            .zip(swap_free)
            .map(|(total, free)| total.saturating_sub(free)),
    })
}

/// 解析 Linux df -Pk 的空间字段；磁盘标签可带空格，因此从行尾读取数值列。
fn parse_linux_disks(output: &str) -> Vec<DiskSnapshot> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 6 {
                return None;
            }
            let offset = fields.len() - 5;
            let total = fields[offset].parse::<u64>().ok()?.saturating_mul(1024);
            let used = fields[offset + 1].parse::<u64>().ok()?.saturating_mul(1024);
            let available = fields[offset + 2].parse::<u64>().ok()?.saturating_mul(1024);
            Some(DiskSnapshot {
                filesystem: fields[..offset].join(" "),
                mount: fields[fields.len() - 1].to_owned(),
                total_bytes: Some(total),
                used_bytes: Some(used),
                available_bytes: Some(available),
            })
        })
        .collect()
}

/// `/proc/net/dev` 固定列序，保留每个网卡累计计数以供下一次采样计算速率。
fn parse_linux_network(output: &str) -> Vec<NetworkCounterSnapshot> {
    output
        .lines()
        .skip(2)
        .filter_map(|line| {
            let (name, values) = line.split_once(':')?;
            let fields: Vec<_> = values.split_whitespace().collect();
            Some(NetworkCounterSnapshot {
                name: name.trim().to_owned(),
                rx_bytes: fields.first()?.parse().ok()?,
                tx_bytes: fields.get(8)?.parse().ok()?,
            })
        })
        .filter(|counter| !counter.name.is_empty())
        .collect()
}

/// ps 状态列用于精确计数正在运行的进程；RSS 从 KiB 换算为字节并限制明细数量。
fn parse_linux_processes(output: &str) -> (Option<u64>, Option<u64>, Vec<ProcessSnapshot>) {
    let mut process_count = 0_u64;
    let mut running_process_count = 0_u64;
    let mut processes = Vec::new();
    for line in output.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let Some(pid) = fields.first().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(state) = fields.get(1) else {
            continue;
        };
        process_count = process_count.saturating_add(1);
        if state.starts_with('R') {
            running_process_count = running_process_count.saturating_add(1);
        }
        let Some(cpu) = fields
            .get(2)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        let Some(memory_percent) = fields
            .get(3)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        let Some(resident) = fields.get(4).and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let name = fields.get(5..).unwrap_or_default().join(" ");
        if name.is_empty() {
            continue;
        }
        processes.push(ProcessSnapshot {
            pid,
            name,
            cpu_percent: Some(cpu),
            memory_percent: Some(memory_percent),
            memory_bytes: Some(resident.saturating_mul(1024)),
        });
    }
    processes.sort_by(|left, right| {
        right
            .cpu_percent
            .partial_cmp(&left.cpu_percent)
            .unwrap_or(Ordering::Equal)
    });
    processes.truncate(100);
    (Some(process_count), Some(running_process_count), processes)
}

/// 百分比字段接受 Docker CLI 的带百分号格式，并拒绝无穷或超范围值。
fn parse_percent(value: &str) -> Option<f64> {
    let percent = value.trim().trim_end_matches('%').parse::<f64>().ok()?;
    (percent.is_finite() && (0.0..=100.0).contains(&percent)).then_some(percent)
}

/// Docker 的人类可读内存值转为字节；单位缺失或格式不明时返回空值。
fn parse_memory_usage(value: &str) -> Option<(Option<u64>, Option<u64>)> {
    let (used, total) = value.split_once('/')?;
    Some((parse_size(used.trim()), parse_size(total.trim())))
}

/// Docker/Podman 的 NetIO 顺序为接收和发送累计字节；只在两项单位都能解析时输出。
fn parse_network_io(value: &str) -> Option<(u64, u64)> {
    let (received, sent) = value.split_once('/')?;
    Some((parse_size(received.trim())?, parse_size(sent.trim())?))
}

/// 转换常见 SI/IEC 单位，避免把未识别单位猜成字节。
fn parse_size(value: &str) -> Option<u64> {
    let split = value.find(|character: char| !(character.is_ascii_digit() || character == '.'))?;
    let number = value[..split].parse::<f64>().ok()?;
    let multiplier = match value[split..].trim().to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0_f64.powi(2),
        "gb" | "gib" => 1024.0_f64.powi(3),
        "tb" | "tib" => 1024.0_f64.powi(4),
        _ => return None,
    };
    let bytes = number * multiplier;
    (bytes.is_finite() && bytes >= 0.0 && bytes <= u64::MAX as f64).then_some(bytes as u64)
}

/// 容器 stats JSON 中字段名随 Docker/Podman 版本存在大小写差异，集中兼容两者。
fn json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

/// sysinfo 的数值统一为 finite 可用值，零内存容量表示 unavailable 而非伪精确数据。
fn finite_percent(value: f32) -> Option<f64> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value as f64)
}

/// 无法区分的零容量保持 None，避免 UI 将无 swap 或不可读容量显示成真实零。
fn positive(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

/// 空白命令输出不作为主机名或系统信息写入快照。
fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// 领域时间以 Unix 毫秒保存，系统时钟早于 epoch 时明确使用零基线。
fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "../../tests/unit/linux_identity.rs"]
mod linux_identity_tests;

#[cfg(test)]
#[path = "../../tests/unit/server_info.rs"]
mod tests;
