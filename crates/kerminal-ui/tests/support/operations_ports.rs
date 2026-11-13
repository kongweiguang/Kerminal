//! 固定 tmux setter 回归的本地端口装配。
//!
//! 所有端口都明确拒绝真实命令、SSH、会话或转发资源；测试只通过真实
//! `OperationsService` 装配和 `OperationsView` 状态断言验证界面副作用。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, RemoteConnectionLease, RemoteConnectionService,
        management::{
            HostDocumentRevision, HostDraft, HostGroupDeletionReceipt, HostGroupDraft,
            HostGroupSnapshot, HostManagementRepository, HostManagementService, ManagedHost,
        },
    },
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        ContainerStageCleanupCapability, ContainerStageCleanupRequest, OperationsCommandExecutor,
        OperationsCommandStream, OperationsService, PortForwardRuntime, PortForwardRuntimeHandle,
        ServerInfoCollector, ServerInfoTier,
    },
    preferences::{ProfileDraft, ProfileRepository, ProfileRevision, VersionedProfile},
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionDeadline, SessionFilter,
        SessionRuntimeMetadata, SessionService, SessionSubscription, ShellCommandOutcome,
        ShellCommandRequest, TerminalCommandContext, TerminalInputEvent, TerminalPalette,
        TerminalSearchRequest, TerminalSearchResult, TerminalSelection, UiCommandLineRequest,
        UiSessionBinding,
    },
};
use kerminal_domain::{
    identity::{HostId, SessionId},
    operations::{PortForwardConfig, ServerInfoSnapshot, ServerInfoTarget},
    preferences::{ConfigRevision, TerminalProfile},
    sessions::{SessionInfo, TerminalSize},
};
use tokio_util::sync::CancellationToken;

// 本地主机快照由测试侧构造后交给内存目录；重新导出避免测试再写一条路径依赖。
pub(super) use kerminal_application::connections::management::ManagedLocalHost;

/// 装配真实用例服务，但把全部 I/O 端口替换为显式失败，避免测试接触 SSH/tmux。
pub(super) fn operations_service() -> Arc<OperationsService> {
    operations_service_with_commands(Arc::new(RejectingExecutor))
}

/// 允许测试只替换 tmux 命令端口，其余 I/O 仍显式失败，保证异步回归经过真实用例服务。
///
/// @author kongweiguang
pub(super) fn operations_service_with_commands(
    commands: Arc<dyn OperationsCommandExecutor>,
) -> Arc<OperationsService> {
    let service = OperationsService::new(
        commands,
        Arc::new(UnusedSessions),
        Arc::new(NoRemote),
        Arc::new(NoProfiles),
        Arc::new(NoServerInfo),
        Arc::new(NoForwards),
    );
    Arc::new(service)
}

/// 装配带内存本地主机目录的真实用例服务；本机 profile 目标必须经生产
/// `HostManagementService::local_session_target` 解析，不能用空目录或假 profile ID 蒙混。
///
/// 目录只登记调用方显式给出的本地主机，其它读写端口继续显式失败，避免测试启动真实
/// shell、SSH 或磁盘配置。
///
/// @author kongweiguang
pub(super) fn operations_service_with_local_hosts(
    commands: Arc<dyn OperationsCommandExecutor>,
    local_hosts: Vec<ManagedLocalHost>,
) -> Arc<OperationsService> {
    let hosts = HostManagementService::new(Arc::new(MemoryLocalHosts { local_hosts }));
    let service = OperationsService::new(
        commands,
        Arc::new(UnusedSessions),
        Arc::new(NoRemote),
        Arc::new(NoProfiles),
        Arc::new(NoServerInfo),
        Arc::new(NoForwards),
    )
    .with_local_host_service(hosts);
    Arc::new(service)
}

/// 只保存测试显式登记的本地主机；远程主机目录、分组和所有写入一律失败，
/// 防止本机 profile 回归顺手访问真实配置目录或借默认模板绕过解析。
///
/// @author kongweiguang
struct MemoryLocalHosts {
    local_hosts: Vec<ManagedLocalHost>,
}

#[async_trait]
impl HostManagementRepository for MemoryLocalHosts {
    /// 本地 tmux 读取只查目录，不申请写门禁。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        unavailable()
    }

    /// 远程主机目录保持空，避免本机回归意外进入 SSH 路径。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(Vec::new())
    }

    /// 本机目标不读取远程主机。
    async fn load_host(&self, _host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(None)
    }

    /// 返回显式登记的本地主机快照，供生产 `local_session_target` 解析 shell 与目录。
    async fn list_local_hosts(&self) -> AppResult<Vec<ManagedLocalHost>> {
        Ok(self.local_hosts.clone())
    }

    /// 按稳定 ID 返回本地主机；缺失以 None 表达，不能回退到默认 profile。
    async fn load_local_host(&self, host_id: &HostId) -> AppResult<Option<ManagedLocalHost>> {
        Ok(self
            .local_hosts
            .iter()
            .find(|host| &host.id == host_id)
            .cloned())
    }

    /// 本机 tmux 回归不保存主机。
    async fn save_host(
        &self,
        _host_id: HostId,
        _draft: HostDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        unavailable()
    }

    /// 本机 tmux 回归不删除主机。
    async fn delete_host(
        &self,
        _host_id: &HostId,
        _expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost> {
        unavailable()
    }

    /// 目录中没有分组。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot::default())
    }

    /// 本机 tmux 回归不保存分组。
    async fn save_group(
        &self,
        _draft: HostGroupDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        unavailable()
    }

    /// 本机 tmux 回归不删除分组。
    async fn delete_group_and_reassign(
        &self,
        _group_id: String,
        _destination_group_id: Option<String>,
        _expected_group_revision: ConfigRevision,
        _expected_hosts: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        unavailable()
    }
}

/// 构造带稳定身份的本地主机快照；调用方用它的 `id` 作为 tmux 本机 profile 目标。
///
/// @author kongweiguang
pub(super) fn local_host_fixture(shell: &str, cwd: Option<&str>) -> ManagedLocalHost {
    ManagedLocalHost {
        id: HostId::new(),
        group_id: None,
        name: "fixture local host".to_owned(),
        shell: shell.to_owned(),
        args: Vec::new(),
        cwd: cwd.map(str::to_owned),
        environment_ref: None,
        sort_order: 0,
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
        revision: ConfigRevision::from_sha256("local-host-revision-1".to_owned()),
        migration_origin: None,
    }
}

/// 只回答明确 tmux argv 的内存命令脚本；任何其它命令、流式命令或资源预留继续失败。
///
/// 用真实 `OperationsService` 驱动 probe/list/kill，避免测试退化为只调用判定谓词；
/// list 可以按主机或调用顺序返回成功/失败，并支持暂停首个列表回执来制造真实旧票据。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct TmuxCommandScript {
    state: Arc<Mutex<TmuxCommandState>>,
    release: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

struct TmuxCommandState {
    probe: TmuxProbeReply,
    probe_replies: VecDeque<TmuxProbeReply>,
    list_replies: VecDeque<TmuxListReply>,
    host_list_replies: HashMap<HostId, TmuxListReply>,
    host_list_sequences: HashMap<HostId, VecDeque<TmuxListReply>>,
    kill_reply: TmuxKillReply,
    detach_reply: TmuxKillReply,
    list_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    list_waiting: bool,
    probe_calls: usize,
    list_calls: usize,
    kill_calls: usize,
    detach_calls: usize,
}

/// 明确区分探测不可用、探测端口失败和真实成功列表，避免把能力缺失当成空结果。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TmuxProbeReply {
    Available,
    Unavailable,
    Error,
}

/// list 结果只区分真实成功（可为空会话）和端口失败；成功才会进入 UI 的权威快照分支。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TmuxListReply {
    Success,
    Error,
}

/// kill 回执显式模拟真实删除或端口失败，测试不再依赖静态布尔分支。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TmuxKillReply {
    Killed,
    Error,
}

impl Default for TmuxCommandState {
    /// 默认能力可用、列表成功、kill 成功；测试按场景覆盖，不隐式接触真实进程。
    fn default() -> Self {
        Self {
            probe: TmuxProbeReply::Available,
            probe_replies: VecDeque::new(),
            list_replies: VecDeque::new(),
            host_list_replies: HashMap::new(),
            host_list_sequences: HashMap::new(),
            kill_reply: TmuxKillReply::Killed,
            detach_reply: TmuxKillReply::Killed,
            list_gate: None,
            list_waiting: false,
            probe_calls: 0,
            list_calls: 0,
            kill_calls: 0,
            detach_calls: 0,
        }
    }
}

impl TmuxCommandScript {
    /// 构造只接受 tmux 命令的内存脚本，默认列表成功返回空集合。
    ///
    /// @author kongweiguang
    pub(super) fn new(probe: TmuxProbeReply) -> Self {
        Self {
            state: Arc::new(Mutex::new(TmuxCommandState {
                probe,
                ..TmuxCommandState::default()
            })),
            release: Arc::new(Mutex::new(None)),
        }
    }

    /// 按调用顺序消费 probe 回执，用于同一目标先不可用、恢复后再成功读取的真实异步回归。
    ///
    /// @author kongweiguang
    pub(super) fn with_probe_replies(
        self,
        replies: impl IntoIterator<Item = TmuxProbeReply>,
    ) -> Self {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .probe_replies = replies.into_iter().collect();
        self
    }

    /// 设置按调用顺序消费的 list 回执；队列耗尽后的意外读取显式失败。
    ///
    /// @author kongweiguang
    pub(super) fn with_list_replies(
        self,
        replies: impl IntoIterator<Item = TmuxListReply>,
    ) -> Self {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .list_replies = replies.into_iter().collect();
        self
    }

    /// 按 SSH host 固定 list 结果，使旧/新目标回执顺序不依赖任务调度先后。
    ///
    /// @author kongweiguang
    pub(super) fn with_host_list_reply(self, host_id: HostId, reply: TmuxListReply) -> Self {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .host_list_replies
            .insert(host_id, reply);
        self
    }

    /// 为同一 SSH host 安排多次 list 回执，覆盖 A→B→A 后失败或恢复再成功。
    ///
    /// @author kongweiguang
    pub(super) fn with_host_list_sequence(
        self,
        host_id: HostId,
        replies: impl IntoIterator<Item = TmuxListReply>,
    ) -> Self {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .host_list_sequences
            .insert(host_id, replies.into_iter().collect());
        self
    }

    /// 设置 kill 结果；成功回执才会触发生产详情门禁。
    ///
    /// @author kongweiguang
    pub(super) fn with_kill_reply(self, reply: TmuxKillReply) -> Self {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .kill_reply = reply;
        self
    }

    /// 设置 detach 结果；成功和失败都必须有归属，不能因换 pane 静默丢弃。
    ///
    /// @author kongweiguang
    pub(super) fn with_detach_reply(self, reply: TmuxKillReply) -> Self {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .detach_reply = reply;
        self
    }

    /// 暂停下一次 list 成功/失败回执，让测试在旧票据尚未完成时发起新请求。
    ///
    /// @author kongweiguang
    pub(super) fn hold_next_list(&self) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().expect("tmux command fixture poisoned");
        state.list_gate = Some(receiver);
        state.list_waiting = false;
        *self.release.lock().expect("tmux command fixture poisoned") = Some(sender);
    }

    /// 等待首个 list 已进入暂停状态，避免在旧请求尚未开始时就释放回执。
    ///
    /// @author kongweiguang
    pub(super) fn list_waiting(&self) -> bool {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .list_waiting
    }

    /// 释放被暂停的 list 回执；只有测试明确推进时才让旧票据完成。
    ///
    /// @author kongweiguang
    pub(super) fn release_list(&self) {
        if let Some(sender) = self
            .release
            .lock()
            .expect("tmux command fixture poisoned")
            .take()
        {
            let _ = sender.send(());
        }
    }

    /// 已执行的真实 probe 次数。
    ///
    /// @author kongweiguang
    pub(super) fn probe_calls(&self) -> usize {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .probe_calls
    }

    /// 已执行的真实 list 次数。
    ///
    /// @author kongweiguang
    pub(super) fn list_calls(&self) -> usize {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .list_calls
    }

    /// 已执行的真实 kill 次数。
    ///
    /// @author kongweiguang
    pub(super) fn kill_calls(&self) -> usize {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .kill_calls
    }

    /// 已执行的真实 detach 次数。
    ///
    /// @author kongweiguang
    pub(super) fn detach_calls(&self) -> usize {
        self.state
            .lock()
            .expect("tmux command fixture poisoned")
            .detach_calls
    }
}

/// 只根据结构化 argv 回答 probe/list/kill；其它参数和目标一律失败，不执行真实命令。
///
/// @author kongweiguang
#[async_trait]
impl OperationsCommandExecutor for TmuxCommandScript {
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("fixture command cancelled"));
        }
        let command = invocation
            .arguments
            .iter()
            .find(|argument| {
                matches!(
                    argument.as_str(),
                    "-V" | "list-sessions" | "kill-session" | "list-windows" | "detach-client"
                )
            })
            .cloned()
            .ok_or_else(|| AppError::unsupported("fixture only serves explicit tmux argv"))?;
        // 所有分支统一返回 Result：失败也先带出门闩再返回，保证暂停的 list 在错误
        // 路径同样先释放锁并等待释放，再统一交出回执，不在锁内存活期间 await。
        let (reply, gate) = {
            let mut state = self.state.lock().expect("tmux command fixture poisoned");
            match command.as_str() {
                "-V" => {
                    state.probe_calls += 1;
                    let reply = state.probe_replies.pop_front().unwrap_or(state.probe);
                    (probe_result(reply), None)
                }
                "list-sessions" => {
                    state.list_calls += 1;
                    state.list_waiting = state.list_gate.is_some();
                    let gate = state.list_gate.take();
                    let reply = match target {
                        CommandExecutionTarget::Host(host_id) => {
                            let sequenced = state
                                .host_list_sequences
                                .get_mut(&host_id)
                                .and_then(VecDeque::pop_front);
                            sequenced
                                .or_else(|| state.host_list_replies.get(&host_id).copied())
                                .unwrap_or(TmuxListReply::Error)
                        }
                        CommandExecutionTarget::Local => state
                            .list_replies
                            .pop_front()
                            .unwrap_or(TmuxListReply::Error),
                    };
                    (list_result(reply), gate)
                }
                "kill-session" => {
                    state.kill_calls += 1;
                    (kill_result(state.kill_reply), None)
                }
                "detach-client" => {
                    state.detach_calls += 1;
                    (detach_result(state.detach_reply), None)
                }
                "list-windows" => (
                    Err(AppError::unsupported("fixture rejects tmux detail IO")),
                    None,
                ),
                _ => unreachable!("command filter only admits known tmux argv"),
            }
        };
        // 门闩在锁释放后等待；被暂停的旧票据必须先收到释放信号，再返回真实回执。
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        reply
    }

    /// 测试不创建流式命令 worker。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        unavailable()
    }

    /// 不预留容器 staging 资源。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        unavailable()
    }

    /// 内存脚本没有后台 worker。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// 没有资源需要异步回收。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 探测只把明确版本输出认作可用；不可用与端口错误保持不同恢复语义。
///
/// @author kongweiguang
fn probe_result(reply: TmuxProbeReply) -> AppResult<CommandExecutionResult> {
    match reply {
        TmuxProbeReply::Available => Ok(command_result(b"tmux 3.4\n", 0)),
        TmuxProbeReply::Unavailable => Ok(command_result(b"", 1)),
        TmuxProbeReply::Error => Err(AppError::unsupported("fixture tmux probe failed")),
    }
}

/// 真实列表成功可以是空集合；端口错误不能伪装成成功空列表。
///
/// @author kongweiguang
fn list_result(reply: TmuxListReply) -> AppResult<CommandExecutionResult> {
    match reply {
        TmuxListReply::Success => Ok(command_result(b"", 0)),
        TmuxListReply::Error => Err(AppError::unsupported("fixture tmux list failed")),
    }
}

/// kill 成功只返回 exit 0，后续详情清理仍由生产 View 的冻结身份门禁决定。
///
/// @author kongweiguang
fn kill_result(reply: TmuxKillReply) -> AppResult<CommandExecutionResult> {
    match reply {
        TmuxKillReply::Killed => Ok(command_result(b"", 0)),
        TmuxKillReply::Error => Err(AppError::unsupported("fixture tmux kill failed")),
    }
}

/// detach 与 kill 共用成功/失败分类，但独立计数，便于断言迟到回执确实到达视图。
///
/// @author kongweiguang
fn detach_result(reply: TmuxKillReply) -> AppResult<CommandExecutionResult> {
    match reply {
        TmuxKillReply::Killed => Ok(command_result(b"", 0)),
        TmuxKillReply::Error => Err(AppError::unsupported("fixture tmux detach failed")),
    }
}

/// 构造无截断、无 stdin 的完整命令回执，避免测试顺手引入未验证的完成标志。
///
/// @author kongweiguang
fn command_result(stdout: &[u8], exit_code: i32) -> CommandExecutionResult {
    CommandExecutionResult {
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
        exit_code: Some(exit_code),
        truncated: false,
        input_complete: true,
    }
}

/// 任何未预期的测试端口调用都失败，而不是退回真实网络或进程。
fn unavailable<T>() -> AppResult<T> {
    Err(AppError::unsupported(
        "operations tmux binding fixture rejects live IO",
    ))
}

struct RejectingExecutor;

#[async_trait]
impl OperationsCommandExecutor for RejectingExecutor {
    /// 显式失败，证明测试没有启动本机或远端 tmux 命令。
    async fn execute(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        unavailable()
    }

    /// 流式命令同样拒绝，避免把意外读取伪装成普通命令。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        unavailable()
    }

    /// 本测试不创建容器 staging 能力。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        unavailable()
    }

    /// 没有后台 worker 需要停止。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// 没有资源需要异步回收。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

struct UnusedSessions;

#[async_trait]
impl SessionService for UnusedSessions {
    /// setter 回归不创建真实终端。
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        unavailable()
    }

    /// 可取消创建同样不启动 PTY；任何意外请求都显式失败。
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unavailable()
    }

    /// 不读取会话目录。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        unavailable()
    }

    /// 不读取运行时会话元数据。
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        unavailable()
    }

    /// 不写入会话字节。
    async fn write(&self, _session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        unavailable()
    }

    /// 不注入终端输入事件。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        unavailable()
    }

    /// 不接受 UI 命令行提交。
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        unavailable()
    }

    /// 不调整终端网格。
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        unavailable()
    }

    /// 不读取渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unavailable()
    }

    /// 不更新终端调色板。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        unavailable()
    }

    /// 不调整 scrollback。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        unavailable()
    }

    /// 不读取纯文本快照。
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        unavailable()
    }

    /// 不滚动终端视口。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        unavailable()
    }

    /// 不执行带 UI owner 约束的滚动。
    async fn scroll_ui(
        &self,
        _binding: UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        unavailable()
    }

    /// 不修改文本选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        unavailable()
    }

    /// 不提取选择文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        unavailable()
    }

    /// 不搜索终端历史。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unavailable()
    }

    /// 不重连会话。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unavailable()
    }

    /// 不关闭单个会话。
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        unavailable()
    }

    /// 不请求带 deadline 的会话回收。
    async fn close_until(
        &self,
        _session_id: SessionId,
        _deadline: SessionDeadline,
    ) -> AppResult<()> {
        unavailable()
    }

    /// 不关闭会话集合。
    async fn close_all(&self) -> AppResult<()> {
        unavailable()
    }

    /// 不订阅会话变化。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        unavailable()
    }

    /// 不发送受管 shell 命令。
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        unavailable()
    }
}

struct NoRemote;

#[async_trait]
impl RemoteConnectionService for NoRemote {
    /// 巡检/读取断言不建立 SSH 连接。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        unavailable()
    }
}

struct NoProfiles;

#[async_trait]
impl ProfileRepository for NoProfiles {
    /// 不读取本机 Profile 目录。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }

    /// 版本化列表同样为空，测试不依赖磁盘配置。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(Vec::new())
    }

    /// 不解析用户 Profile。
    async fn get_profile(&self, _id: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(None)
    }

    /// 不保存 Profile。
    async fn save_profile(
        &self,
        _draft: ProfileDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        unavailable()
    }

    /// 不删除 Profile。
    async fn delete_profile(
        &self,
        _id: &str,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        unavailable()
    }

    /// 不修改默认 Profile。
    async fn set_default_profile(
        &self,
        _id: &str,
        _expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        unavailable()
    }
}

struct NoServerInfo;

#[async_trait]
impl ServerInfoCollector for NoServerInfo {
    /// 不启动主机资源探针。
    async fn collect(
        &self,
        _target: ServerInfoTarget,
        _tier: ServerInfoTier,
        _cancel: CancellationToken,
    ) -> AppResult<ServerInfoSnapshot> {
        unavailable()
    }
}

struct NoForwards;

#[async_trait]
impl PortForwardRuntime for NoForwards {
    /// 不创建真实 listener、SOCKS 或 SSH 隧道。
    async fn start(
        &self,
        _id: String,
        _config: PortForwardConfig,
        _cancel: CancellationToken,
    ) -> AppResult<Arc<dyn PortForwardRuntimeHandle>> {
        unavailable()
    }
}
