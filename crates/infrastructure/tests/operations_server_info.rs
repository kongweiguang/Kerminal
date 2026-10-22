//! 系统资源采样 adapter 的平台隔离测试。
//!
//! @author kongweiguang

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyTrustReceipt, HostKeyTrustRequest,
        RemoteConnectionLease, RemoteConnectionService,
    },
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        ContainerStageCleanupCapability, ContainerStageCleanupRequest, OperationsCommandExecutor,
        OperationsCommandStream, ServerInfoCollector, ServerInfoTier,
    },
};
use kerminal_domain::{files::ContainerRuntime, identity::HostId, operations::ServerInfoTarget};
use kerminal_infrastructure::operations::SystemServerInfoCollector;
use tokio_util::sync::CancellationToken;

/// 收拢脚本化命令响应队列的共享类型，避免测试替身字段重复暴露同步容器细节。
///
/// @author kongweiguang
type CommandResponseQueue =
    Arc<Mutex<VecDeque<(Option<String>, AppResult<CommandExecutionResult>)>>>;

#[derive(Clone)]
struct CommandResponse {
    responses: CommandResponseQueue,
    calls: Arc<Mutex<Vec<(CommandExecutionTarget, CommandInvocation)>>>,
}

impl CommandResponse {
    /// 一次性返回结构化命令结果并记录 argv，禁止测试通过启动真实容器服务。
    fn new(result: AppResult<CommandExecutionResult>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from([(None, result)]))),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 按固定程序顺序回放结果；容器 exec 项以 argv 中的内层命令名标识。
    ///
    /// @author kongweiguang
    fn scripted(responses: Vec<(&str, AppResult<CommandExecutionResult>)>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(
                responses
                    .into_iter()
                    .map(|(program, response)| (Some(program.to_owned()), response))
                    .collect(),
            )),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// 仅将 Docker/Podman `exec --` 的内层命令名用于脚本匹配，保留外层 stats 的 runtime 名。
///
/// @author kongweiguang
fn container_exec_program(invocation: &CommandInvocation) -> Option<&str> {
    if !matches!(invocation.program.as_str(), "docker" | "podman")
        || invocation.arguments.first().map(String::as_str) != Some("exec")
        || invocation.arguments.get(1).map(String::as_str) != Some("--")
    {
        return None;
    }

    invocation.arguments.get(3).map(String::as_str)
}

#[async_trait]
impl OperationsCommandExecutor for CommandResponse {
    /// 按调用边界匹配回放结果：runtime 命令匹配外层程序，exec 探针匹配固定 argv 的内层命令。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        let program = invocation.program.clone();
        let scripted_program = container_exec_program(&invocation)
            .unwrap_or(program.as_str())
            .to_owned();
        self.calls
            .lock()
            .expect("call mutex poisoned")
            .push((target, invocation));
        let (expected_program, response) = self
            .responses
            .lock()
            .expect("result mutex poisoned")
            .pop_front()
            .ok_or_else(|| AppError::infrastructure("test command result was already consumed"))?;
        if expected_program
            .as_deref()
            .is_some_and(|expected| expected != scripted_program.as_str())
        {
            return Err(AppError::infrastructure(
                "test command program did not match the scripted response",
            ));
        }
        response
    }

    /// 采样不写 stdin，测试执行器明确拒绝未规划的输入型命令。
    async fn execute_with_input(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _input: Vec<u8>,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        Err(AppError::unsupported("test executor has no stdin"))
    }

    /// 采样 fixture 仅回放有限 JSON，不创建长期双向命令流资源。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        Err(AppError::unsupported("test executor has no command stream"))
    }

    /// 只读采样 fixture 不拥有容器 staging 资源，显式拒绝签发清理能力。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        Err(AppError::unsupported(
            "test executor does not create container staging resources",
        ))
    }

    /// 该 fixture 没有后台资源，退出前 quiesce 无额外动作。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// 此 fixture 不创建后台任务，因此其绝对截止清理没有资源需要 join。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

struct NoRemoteConnections;

#[async_trait]
impl RemoteConnectionService for NoRemoteConnections {
    /// 当前场景仅测试本机和容器采样，不允许 fixture 意外建立真实 SSH 连接。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("test SSH connection is disabled"))
    }

    /// 测试不接受 host key 挑战，避免 fixture 写入用户 trust store。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("test SSH connection is disabled"))
    }

    /// 测试不修改 known_hosts。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("test SSH connection is disabled"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

/// Docker stats 的人类可读单位应转为字节，并保留计数缺失时的空值语义。
#[tokio::test]
async fn container_stats_are_parsed_without_opening_a_terminal() {
    let commands = CommandResponse::new(Ok(CommandExecutionResult {
        stdout: br#"{"ID":"abc123","CPUPerc":"12.50%","MemPerc":"25.00%","MemUsage":"256MiB / 1GiB","PIDs":"4","NetIO":"10kB / 12kB"}"#.to_vec(),
        stderr: Vec::new(),
        exit_code: Some(0),
        truncated: false,
        input_complete: true,
    }));
    let collector =
        SystemServerInfoCollector::new(Arc::new(commands.clone()), Arc::new(NoRemoteConnections));
    let snapshot = collector
        .collect(
            ServerInfoTarget::Container {
                host_id: None,
                runtime: ContainerRuntime::Docker,
                container_id: "abc123".to_owned(),
            },
            ServerInfoTier::Fast,
            CancellationToken::new(),
        )
        .await
        .expect("container sample should parse");

    assert_eq!(snapshot.cpu.usage_percent, Some(12.5));
    assert_eq!(snapshot.memory.total_bytes, Some(1024 * 1024 * 1024));
    assert_eq!(snapshot.memory.used_bytes, Some(256 * 1024 * 1024));
    assert_eq!(snapshot.process_count, Some(4));
    assert_eq!(snapshot.running_process_count, None);
    assert!(snapshot.sample_times.fast_at_unix_ms.is_some());
    assert!(snapshot.sample_times.slow_at_unix_ms.is_none());
    assert_eq!(snapshot.network_counters.len(), 1);
    assert_eq!(snapshot.network_counters[0].rx_bytes, 10 * 1024);
    assert_eq!(snapshot.network_counters[0].tx_bytes, 12 * 1024);
    assert_eq!(
        snapshot.network_probe_status,
        kerminal_domain::operations::ServerInfoProbeStatus::Available
    );
    let calls = commands.calls.lock().expect("call mutex poisoned");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.program, "docker");
    assert!(calls[0].1.arguments.contains(&"--no-stream".to_owned()));
}

/// 静态层应读取带标签的 Linux 身份与 hostname，并且不触发 stats 或慢速容器资源探针。
///
/// @author kongweiguang
#[tokio::test]
async fn container_static_tier_does_not_issue_runtime_stats_probe() {
    let commands = CommandResponse::scripted(vec![
        (
            "sh",
            Ok(CommandExecutionResult {
                stdout: b"os=Linux\narchitecture=x86_64\nkernel=6.1.0\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "hostname",
            Ok(CommandExecutionResult {
                stdout: b"demo-container\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
    ]);
    let collector =
        SystemServerInfoCollector::new(Arc::new(commands.clone()), Arc::new(NoRemoteConnections));
    let snapshot = collector
        .collect(
            ServerInfoTarget::Container {
                host_id: None,
                runtime: ContainerRuntime::Docker,
                container_id: "abc123".to_owned(),
            },
            ServerInfoTier::Static,
            CancellationToken::new(),
        )
        .await
        .expect("static container projection should be available");

    assert!(snapshot.sample_times.fast_at_unix_ms.is_none());
    assert!(snapshot.sample_times.static_info_at_unix_ms.is_some());
    assert_eq!(snapshot.hostname.as_deref(), Some("demo-container"));
    assert_eq!(snapshot.operating_system.as_deref(), Some("Linux"));
    assert_eq!(snapshot.architecture.as_deref(), Some("x86_64"));
    assert_eq!(snapshot.kernel.as_deref(), Some("6.1.0"));
    assert_eq!(
        snapshot.network_probe_status,
        kerminal_domain::operations::ServerInfoProbeStatus::NotSampled
    );
    let calls = commands.calls.lock().expect("call mutex poisoned");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls
            .iter()
            .map(|(_, invocation)| container_exec_program(invocation))
            .collect::<Vec<_>>(),
        [Some("sh"), Some("hostname")]
    );
    assert_eq!(calls[0].1.arguments.get(4).map(String::as_str), Some("-c"));
    let identity_script = calls[0]
        .1
        .arguments
        .get(5)
        .expect("identity probe should pass its script to sh -c");
    assert!(identity_script.contains("os="));
    assert!(identity_script.contains("architecture="));
    assert!(identity_script.contains("kernel="));
    assert!(calls.iter().all(|(_, invocation)| {
        invocation.program == "docker" && !invocation.arguments.iter().any(|arg| arg == "stats")
    }));
}

/// sh 命令匹配成功但未返回带标签的 Linux 标识时，静态采样仍必须拒绝。
///
/// @author kongweiguang
#[tokio::test]
async fn container_static_tier_rejects_unparseable_system_identity() {
    let commands = CommandResponse::scripted(vec![
        (
            "sh",
            Ok(CommandExecutionResult {
                stdout: br#"{"ID":"abc123"}"#.to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "hostname",
            Ok(CommandExecutionResult {
                stdout: b"demo-container\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
    ]);
    let collector =
        SystemServerInfoCollector::new(Arc::new(commands.clone()), Arc::new(NoRemoteConnections));

    let result = collector
        .collect(
            ServerInfoTarget::Container {
                host_id: None,
                runtime: ContainerRuntime::Docker,
                container_id: "abc123".to_owned(),
            },
            ServerInfoTier::Static,
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(
        result,
        Err(error) if error.public_code() == kerminal_application::PublicErrorCode::Unsupported
    ));
    assert_eq!(
        commands
            .calls
            .lock()
            .expect("call mutex poisoned")
            .iter()
            .map(|(_, invocation)| container_exec_program(invocation))
            .collect::<Vec<_>>(),
        [Some("sh"), Some("hostname")]
    );
}

/// 容器 Full 层通过 runtime exec 采集磁盘、网卡、进程和设备，并分别记录探针状态。
///
/// @author kongweiguang
#[tokio::test]
async fn container_full_tier_collects_static_slow_and_device_details() {
    let commands = CommandResponse::scripted(vec![
        (
            "docker",
            Ok(CommandExecutionResult {
                stdout: br#"{"CPUPerc":"12.50%","MemUsage":"256MiB / 1GiB","MemPerc":"25.00%","PIDs":"4","NetIO":"10kB / 12kB"}"#.to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "sh",
            Ok(CommandExecutionResult {
                stdout: b"os=Linux\narchitecture=x86_64\nkernel=6.1.0\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "hostname",
            Ok(CommandExecutionResult {
                stdout: b"demo-container\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "df",
            Ok(CommandExecutionResult {
                stdout: b"Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 1024 256 768 25% /\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "cat",
            Ok(CommandExecutionResult {
                stdout: b"Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "ps",
            Ok(CommandExecutionResult {
                stdout: b"101 R 1.5 2.0 64 bash\n102 S 0.0 0.5 32 sshd\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "nvidia-smi",
            Ok(CommandExecutionResult {
                stdout: b"NVIDIA RTX 4090, 555.42, 24576, 6144, 36.5, 54, 220.1\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
        (
            "npu-smi",
            Ok(CommandExecutionResult {
                stdout: b"| 2 910B4 | OK | 84 W 44 C |\n| 0 | 0000:06:00.0 | 0 %, 4 / 8 MB, 2657 / 32768 MB |\n".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                truncated: false,
                input_complete: true,
            }),
        ),
    ]);
    let collector =
        SystemServerInfoCollector::new(Arc::new(commands.clone()), Arc::new(NoRemoteConnections));
    let snapshot = collector
        .collect(
            ServerInfoTarget::Container {
                host_id: None,
                runtime: ContainerRuntime::Docker,
                container_id: "abc123".to_owned(),
            },
            ServerInfoTier::Full,
            CancellationToken::new(),
        )
        .await
        .expect("full container sample should retain supported data");

    assert_eq!(snapshot.hostname.as_deref(), Some("demo-container"));
    assert_eq!(snapshot.operating_system.as_deref(), Some("Linux"));
    assert_eq!(snapshot.architecture.as_deref(), Some("x86_64"));
    assert_eq!(snapshot.kernel.as_deref(), Some("6.1.0"));
    assert_eq!(snapshot.disks.len(), 1);
    assert_eq!(snapshot.disks[0].total_bytes, Some(1024 * 1024));
    assert_eq!(snapshot.network_counters[0].name, "eth0");
    assert_eq!(snapshot.network_counters[0].rx_bytes, 100);
    assert_eq!(snapshot.process_count, Some(2));
    assert_eq!(snapshot.running_process_count, Some(1));
    assert_eq!(snapshot.gpus[0].vendor.as_deref(), Some("NVIDIA"));
    assert_eq!(snapshot.npus[0].chip_id, Some(0));
    assert_eq!(
        snapshot.gpu_probe_status,
        kerminal_domain::operations::ServerInfoProbeStatus::Available
    );
    assert_eq!(
        snapshot.npu_probe_status,
        kerminal_domain::operations::ServerInfoProbeStatus::Available
    );
    assert!(snapshot.sample_times.static_info_at_unix_ms.is_some());
    assert!(snapshot.sample_times.slow_at_unix_ms.is_some());
    assert!(snapshot.sample_times.gpu_at_unix_ms.is_some());
    assert!(snapshot.sample_times.npu_at_unix_ms.is_some());
    let calls = commands.calls.lock().expect("call mutex poisoned");
    assert_eq!(calls.len(), 8);
    assert_eq!(calls[0].1.program, "docker");
    assert_eq!(
        calls[0].1.arguments.first().map(String::as_str),
        Some("stats")
    );
    assert_eq!(
        calls
            .iter()
            .skip(1)
            .map(|(_, invocation)| container_exec_program(invocation))
            .collect::<Vec<_>>(),
        [
            Some("sh"),
            Some("hostname"),
            Some("df"),
            Some("cat"),
            Some("ps"),
            Some("nvidia-smi"),
            Some("npu-smi"),
        ]
    );
}

/// 非零 Docker/Podman 退出应作为真实失败传播，不以空 metrics 冒充采样成功。
#[tokio::test]
async fn failed_container_stats_remain_a_typed_error() {
    let commands = CommandResponse::new(Ok(CommandExecutionResult {
        stdout: Vec::new(),
        stderr: b"container not found".to_vec(),
        exit_code: Some(1),
        truncated: false,
        input_complete: true,
    }));
    let collector =
        SystemServerInfoCollector::new(Arc::new(commands), Arc::new(NoRemoteConnections));
    let result = collector
        .collect(
            ServerInfoTarget::Container {
                host_id: None,
                runtime: ContainerRuntime::Podman,
                container_id: "missing".to_owned(),
            },
            ServerInfoTier::Fast,
            CancellationToken::new(),
        )
        .await;

    assert!(
        matches!(result, Err(error) if error.public_code() == kerminal_application::PublicErrorCode::RemoteExit)
    );
}
