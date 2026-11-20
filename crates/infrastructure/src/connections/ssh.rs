//! Russh SSH service：连接策略集中在此处，各业务能力只创建独立 SSH channel。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        AsyncDuplex, CommandEvent, CommandStreamHandle, ConnectionIntent, CredentialResolver,
        HostConnectionRepository, HostKeyIdentity, HostKeyStatus, KnownHostsRepository,
        OpenSftpRequest, RemoteConnectionHandle, RemoteConnectionLease, RemoteConnectionService,
        RemoteForwardHandle, RemoteForwardListener, RemoteShellDialect, RemoteShellKind,
        ResolvedCredential, SftpChannelHandle, ShellScriptRequest, TerminalOpenRequest,
        TunnelChannelHandle, TunnelRequest,
    },
    sessions::SessionTransport,
};
use kerminal_domain::{
    connections::{
        ConnectionCapabilities, HostConnectionProfile, HostEndpoint, RemoteProtocol,
        SshAuthentication,
        credentials::{CredentialMaterial, CredentialReference},
    },
    identity::HostId,
};
use russh::{
    ChannelMsg,
    client::{self, AuthResult, Handle, Msg},
    keys::{
        HashAlg, PrivateKeyWithHashAlg,
        agent::client::{AgentClient, AgentStream},
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf},
    sync::{Mutex, RwLock, mpsc},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    exec::{encode_exec_command, encode_shell_script},
    proxy::connect_via_proxy,
    terminal::make_ssh_terminal_transport,
};

mod command_stream;
#[path = "ssh_cleanup.rs"]
mod ssh_cleanup;
pub use ssh_cleanup::RusshConnectionCleanupStatus;
use ssh_cleanup::{
    HandshakeRegistrationGuard, PendingHandshakeCleanup, PendingRusshCleanup,
    RusshCleanupReservation, RusshPeerLifetimeOwner, SessionSetGuard,
};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_JUMP_HOSTS: usize = 8;
const MAX_SUBSYSTEM_SETUP_BYTES: usize = 1024 * 1024;
const MAX_PRIVATE_KEY_BYTES: usize = 1024 * 1024;
const MAX_SHELL_PROBE_OUTPUT_BYTES: usize = 512;
const SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const UNKNOWN_DYNAMIC_REMOTE_FORWARD_MESSAGE: &str =
    "此前动态转发建立结果未知，请关闭此连接并重新连接后重试";

/// 将持久配置、凭据库和 known_hosts 注入 SSH adapter，避免基础设施自行决定存储位置。
pub struct RusshRemoteConnectionDependencies {
    pub hosts: Arc<dyn HostConnectionRepository>,
    pub credentials: Arc<dyn CredentialResolver>,
    pub known_hosts: Arc<dyn KnownHostsRepository>,
}

/// 通过真实 russh SSH 会话实现应用层共享连接端口。
pub struct RusshRemoteConnectionService {
    dependencies: Arc<RusshRemoteConnectionDependencies>,
    pending_cleanup: Arc<PendingRusshCleanup>,
    accepting_connections: AtomicBool,
    intake_gate: RwLock<()>,
}

impl RusshRemoteConnectionService {
    /// 只接收应用端口；不持有 UI、MCP 或文件系统的全局状态。
    pub fn new(dependencies: RusshRemoteConnectionDependencies) -> Self {
        Self {
            dependencies: Arc::new(dependencies),
            pending_cleanup: PendingRusshCleanup::new(),
            accepting_connections: AtomicBool::new(true),
            intake_gate: RwLock::new(()),
        }
    }

    /// 返回活跃路由、待回收资源及有界准入配额的快照，供桌面退出协调使用。
    pub fn cleanup_status(&self) -> RusshConnectionCleanupStatus {
        self.pending_cleanup
            .status(self.accepting_connections.load(Ordering::Acquire))
    }

    /// 停止新 SSH 请求；调用方须先关闭消费者 lease/channel，再用同一 deadline 重试 drain。
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.accepting_connections.store(false, Ordering::Release);
        let _gate = tokio::time::timeout_at(deadline, self.intake_gate.write())
            .await
            .map_err(|_| AppError::timeout("SSH service intake shutdown"))?;
        drop(_gate);
        self.pending_cleanup.drain_until(deadline).await
    }

    /// Serialize shutdown against in-flight connection setup while allowing independent operations.
    async fn enter(&self) -> AppResult<tokio::sync::RwLockReadGuard<'_, ()>> {
        let guard = self.intake_gate.read().await;
        if !self.accepting_connections.load(Ordering::Acquire) {
            return Err(AppError::cancelled("SSH service is shutting down"));
        }
        Ok(guard)
    }
}

#[async_trait]
impl RemoteConnectionService for RusshRemoteConnectionService {
    /// 使用路由配额覆盖建立中、活动和待回收连接，避免遗漏独立 SFTP/exec。
    fn activity(&self) -> AppResult<kerminal_application::connections::RemoteConnectionActivity> {
        let status = self.cleanup_status();
        Ok(
            kerminal_application::connections::RemoteConnectionActivity {
                routes: status.reserved_route_owners,
                generation: status.route_generation,
            },
        )
    }
    /// 先读 profile 并校验协议能力，再解密凭据和联网；SFTP-only 主机不会触发 SSH shell 请求。
    async fn connect(
        &self,
        host_id: HostId,
        intent: ConnectionIntent,
        cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        let _intake = self.enter().await?;
        let profile = self.dependencies.hosts.load(&host_id).await?;
        let capabilities = ConnectionCapabilities::for_protocol(profile.protocol);
        require_capability(capabilities, intent)?;
        require_ssh_protocol(&profile)?;
        validate_profile(&profile)?;
        let reservation = self.pending_cleanup.reserve()?;
        let forward_router = Arc::new(ForwardRouter::default());
        let sessions = connect_route(
            &profile,
            &self.dependencies,
            Arc::clone(&forward_router),
            Arc::clone(&self.pending_cleanup),
            Arc::clone(&reservation),
            &cancel,
        )
        .await?;
        let Some(target) = sessions.last().cloned() else {
            let _ = disconnect_sessions(
                &sessions,
                &self.pending_cleanup,
                &reservation,
                "Kerminal SSH route created no target",
            )
            .await;
            reservation.finish_route();
            return Err(AppError::infrastructure(
                "SSH route did not create a target connection",
            ));
        };
        let closed = Arc::new(AtomicBool::new(false));
        let lifetime_owner = RusshPeerLifetimeOwner::new(
            Arc::clone(&self.pending_cleanup),
            Arc::clone(&reservation),
            sessions.clone(),
            Arc::clone(&closed),
        );
        let probe_shell = intent == ConnectionIntent::Terminal
            || (intent == ConnectionIntent::Exec
                && profile.shell_dialect == RemoteShellDialect::Unknown);
        let (shell_kind, shell_dialect) = if probe_shell {
            match probe_remote_shell(&target, &cancel).await {
                Ok(Some(detected)) => (detected.kind, detected.dialect),
                Ok(None) => (RemoteShellKind::Unknown, profile.shell_dialect),
                Err(_) if cancel.is_cancelled() => {
                    let _ = disconnect_sessions(
                        &sessions,
                        &self.pending_cleanup,
                        &reservation,
                        "Kerminal shell probe cancelled",
                    )
                    .await;
                    return Err(AppError::cancelled("SSH shell capability probe cancelled"));
                }
                Err(_) => (RemoteShellKind::Unknown, profile.shell_dialect),
            }
        } else {
            (RemoteShellKind::Unknown, profile.shell_dialect)
        };
        let peer = Arc::new(RusshPeer {
            host_id: profile.host_id.clone(),
            capabilities,
            shell_dialect,
            shell_kind,
            operation_timeout: connect_timeout(profile.connect_timeout_ms),
            target,
            sessions,
            forward_router,
            closing: Arc::new(AtomicBool::new(false)),
            closed,
            close_lock: Arc::new(Mutex::new(())),
            shutdown: CancellationToken::new(),
            lifetime_owner,
        });
        Ok(RemoteConnectionLease::new(peer))
    }
}

#[derive(Clone, Copy)]
struct DetectedShell {
    kind: RemoteShellKind,
    dialect: RemoteShellDialect,
}

/// 认证后仅执行常量只读探针；连接内缓存结果供安全参数编码和 shell bootstrap 使用。
async fn probe_remote_shell(
    session: &Arc<RusshPeerSession>,
    cancel: &CancellationToken,
) -> AppResult<Option<DetectedShell>> {
    let deadline = tokio::time::Instant::now() + SHELL_PROBE_TIMEOUT;
    let nonce = Uuid::new_v4().simple().to_string();
    let bash_marker = format!("KERM_BASH_{nonce}");
    let zsh_marker = format!("KERM_ZSH_{nonce}");
    let posix_marker = format!("KERM_POSIX_{nonce}");
    let fish_marker = format!("KERM_FISH_{nonce}");
    let powershell_marker = format!("KERM_POWERSHELL_{nonce}");
    let posix_command = format!(
        "if [ -n \"${{BASH_VERSION-}}\" ]; then printf '%s\\n' '{bash_marker}'; elif [ -n \"${{ZSH_VERSION-}}\" ]; then printf '%s\\n' '{zsh_marker}'; else printf '%s\\n' '{posix_marker}'; fi"
    );
    let posix_deadline = (tokio::time::Instant::now() + Duration::from_secs(1)).min(deadline);
    if let Some(output) =
        run_shell_probe_command(session, &posix_command, cancel, posix_deadline, deadline).await?
    {
        if output_has_exact_marker(&output, &bash_marker) {
            return Ok(Some(DetectedShell {
                kind: RemoteShellKind::Bash,
                dialect: RemoteShellDialect::Posix,
            }));
        }
        if output_has_exact_marker(&output, &zsh_marker) {
            return Ok(Some(DetectedShell {
                kind: RemoteShellKind::Zsh,
                dialect: RemoteShellDialect::Posix,
            }));
        }
        if output_has_exact_marker(&output, &posix_marker) {
            return Ok(Some(DetectedShell {
                kind: RemoteShellKind::Other,
                dialect: RemoteShellDialect::Posix,
            }));
        }
    }

    if tokio::time::Instant::now() >= deadline {
        return Ok(None);
    }
    let fish_command = format!("if set -q FISH_VERSION; printf '%s\\n' '{fish_marker}'; end");
    let fish_deadline = (tokio::time::Instant::now() + Duration::from_secs(1))
        .min(deadline - Duration::from_millis(100));
    if let Some(output) =
        run_shell_probe_command(session, &fish_command, cancel, fish_deadline, deadline).await?
        && output_has_exact_marker(&output, &fish_marker)
    {
        return Ok(Some(DetectedShell {
            kind: RemoteShellKind::Fish,
            dialect: RemoteShellDialect::Fish,
        }));
    }
    if tokio::time::Instant::now() >= deadline {
        return Ok(None);
    }
    let powershell_command =
        format!("if ($PSVersionTable) {{ [Console]::Out.WriteLine('{powershell_marker}') }}");
    let powershell_deadline = deadline
        .checked_sub(Duration::from_millis(100))
        .unwrap_or(deadline);
    let output = run_shell_probe_command(
        session,
        &powershell_command,
        cancel,
        powershell_deadline,
        deadline,
    )
    .await?;
    if output
        .as_deref()
        .is_some_and(|bytes| output_has_exact_marker(bytes, &powershell_marker))
    {
        Ok(Some(DetectedShell {
            kind: RemoteShellKind::PowerShell,
            dialect: RemoteShellDialect::PowerShell,
        }))
    } else {
        Ok(None)
    }
}

/// 打开临时 exec channel 并在单一 deadline 下限制响应大小，探测失败不妨碍普通 shell 登录。
async fn run_shell_probe_command(
    session: &Arc<RusshPeerSession>,
    command: &str,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
    cleanup_deadline: tokio::time::Instant,
) -> AppResult<Option<Vec<u8>>> {
    let handle = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SSH shell probe cancelled")),
        result = tokio::time::timeout_at(deadline, session.handle.lock()) => match result {
            Ok(handle) => handle,
            Err(_) => return Ok(None),
        },
    };
    let mut channel = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SSH shell probe cancelled")),
        result = tokio::time::timeout_at(deadline, handle.channel_open_session()) => match result {
            Ok(Ok(channel)) => channel,
            Ok(Err(_)) | Err(_) => return Ok(None),
        },
    };
    drop(handle);
    let request = tokio::select! {
        _ = cancel.cancelled() => {
            close_probe_channel(&mut channel, cleanup_deadline).await;
            return Err(AppError::cancelled("SSH shell probe cancelled"));
        }
        result = tokio::time::timeout_at(deadline, channel.exec(true, command.as_bytes().to_vec())) => result,
    };
    if !matches!(request, Ok(Ok(()))) {
        close_probe_channel(&mut channel, cleanup_deadline).await;
        return Ok(None);
    }

    let mut accepted = false;
    let mut exit_code = None;
    let mut saw_signal = false;
    let mut output = Vec::new();
    let mut total_bytes = 0usize;
    let mut exit_received = false;
    loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => {
                close_probe_channel(&mut channel, cleanup_deadline).await;
                return Err(AppError::cancelled("SSH shell probe cancelled"));
            }
            result = tokio::time::timeout_at(deadline, channel.wait()) => match result {
                Ok(message) => message,
                Err(_) => {
                    close_probe_channel(&mut channel, cleanup_deadline).await;
                    return Ok(None);
                }
            },
        };
        match message {
            Some(ChannelMsg::Success) => accepted = true,
            Some(ChannelMsg::Failure) => {
                close_probe_channel(&mut channel, cleanup_deadline).await;
                return Ok(None);
            }
            Some(ChannelMsg::Data { data }) => {
                total_bytes = total_bytes.saturating_add(data.len());
                if total_bytes > MAX_SHELL_PROBE_OUTPUT_BYTES {
                    close_probe_channel(&mut channel, cleanup_deadline).await;
                    return Ok(None);
                }
                output.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                total_bytes = total_bytes.saturating_add(data.len());
                if total_bytes > MAX_SHELL_PROBE_OUTPUT_BYTES {
                    close_probe_channel(&mut channel, cleanup_deadline).await;
                    return Ok(None);
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = Some(exit_status);
                exit_received = true;
            }
            Some(ChannelMsg::ExitSignal { .. }) => {
                saw_signal = true;
                exit_received = true;
            }
            Some(ChannelMsg::Close) | None => break,
            Some(ChannelMsg::Eof) => {}
            Some(_) => {}
        }
    }
    if !accepted || !exit_received || exit_code != Some(0) || saw_signal {
        return Ok(None);
    }
    Ok(Some(output))
}

/// 在探测 deadline 仍有余量时显式关闭临时 channel，避免留下独立 exec 请求。
async fn close_probe_channel(channel: &mut russh::Channel<Msg>, deadline: tokio::time::Instant) {
    let close_deadline = (tokio::time::Instant::now() + Duration::from_millis(100)).min(deadline);
    if tokio::time::Instant::now() < close_deadline {
        let _ = tokio::time::timeout_at(close_deadline, channel.close()).await;
    }
}

/// 只接受完整 stdout 行与随机 nonce 完全相等的标记，忽略 shell 启动脚本的其它输出。
fn output_has_exact_marker(output: &[u8], marker: &str) -> bool {
    output.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line == marker.as_bytes()
    })
}

/// 保持一条到 target 的完整 jump chain；Russh 的 handler 会把服务端端口转发 channel 路由到本连接。
pub(super) struct RusshPeer {
    pub(super) host_id: HostId,
    pub(super) capabilities: ConnectionCapabilities,
    pub(super) shell_dialect: RemoteShellDialect,
    shell_kind: RemoteShellKind,
    operation_timeout: Duration,
    target: Arc<RusshPeerSession>,
    sessions: Vec<Arc<RusshPeerSession>>,
    forward_router: Arc<ForwardRouter>,
    closing: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    close_lock: Arc<Mutex<()>>,
    shutdown: CancellationToken,
    lifetime_owner: Arc<RusshPeerLifetimeOwner>,
}

struct RusshPeerSession {
    handle: Mutex<Handle<RusshHandler>>,
    endpoint: HostEndpoint,
    disconnected: AtomicBool,
    /// disconnect 已入队后跨 close 重试保留，避免再次发送或误判稍后到达的完成 sentinel。
    disconnect_requested: AtomicBool,
    transport_cancel: CancellationToken,
}

/// 只标识本应用主动取消 socket 的完成原因，不能把同类 OS 网络错误归为成功关闭。
#[derive(Debug)]
struct LocalSshTransportCancelled;

impl std::fmt::Display for LocalSshTransportCancelled {
    /// 保持稳定且不含连接地址或命令的底层诊断文字。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SSH transport was cancelled locally")
    }
}

impl std::error::Error for LocalSshTransportCancelled {}

/// 在 IO source 中保留私有类型，供 join 后精确区分本地取消与真正的 transport 故障。
///
/// @author kongweiguang
fn local_ssh_transport_cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, LocalSshTransportCancelled)
}

/// 在底层 socket 上合并连接关闭信号，使被取消的 Russh task 能退出而不是只丢弃 Handle。
struct CancellablePeerStream {
    inner: Box<dyn AsyncDuplex>,
    cancel: CancellationToken,
    cancel_wait: Pin<Box<dyn Future<Output = ()> + Send>>,
    cancelled: bool,
}

impl CancellablePeerStream {
    /// 预先注册 cancellation waker，保证当前被网络 I/O 挂起的 Russh task 可被 close 唤醒。
    fn new(inner: Box<dyn AsyncDuplex>, cancel: CancellationToken) -> Self {
        Self {
            inner,
            cancel_wait: Box::pin(cancel.clone().cancelled_owned()),
            cancel,
            cancelled: false,
        }
    }

    /// 在 I/O 被 poll 前检查一次持久 cancellation future，已取消后不重复 poll 完成 future。
    fn poll_cancelled(&mut self, context: &mut Context<'_>) -> bool {
        if self.cancelled || self.cancel.is_cancelled() {
            self.cancelled = true;
            return true;
        }
        if self.cancel_wait.as_mut().poll(context).is_ready() {
            self.cancelled = true;
            return true;
        }
        false
    }
}

impl AsyncRead for CancellablePeerStream {
    /// close/cancel 优先于网络读取完成，并保留应用私有取消 source 供真实 join 后分类。
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.poll_cancelled(context) {
            return Poll::Ready(Err(local_ssh_transport_cancelled_error()));
        }
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for CancellablePeerStream {
    /// 写入与读取共用可识别的取消 source，不能把任意 ConnectionAborted 当成应用取消。
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.poll_cancelled(context) {
            return Poll::Ready(Err(local_ssh_transport_cancelled_error()));
        }
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    /// flush 与数据 IO 使用同一取消证据，未取消时仍直接交给底层 socket。
    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.poll_cancelled(context) {
            return Poll::Ready(Err(local_ssh_transport_cancelled_error()));
        }
        Pin::new(&mut self.inner).poll_flush(context)
    }

    /// shutdown 也绑定到相同 typed 取消信号，保持强制收尾有界且可追溯。
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.poll_cancelled(context) {
            return Poll::Ready(Err(local_ssh_transport_cancelled_error()));
        }
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[async_trait]
impl RemoteConnectionHandle for RusshPeer {
    /// 该连接只绑定创建时解析出的主机 ID，不允许调用方在 lease 内切换目标。
    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// 能力来自已验证的目标协议，连接完成后不再根据调用方请求放宽。
    fn capabilities(&self) -> ConnectionCapabilities {
        self.capabilities
    }

    /// 使用 host profile 中可信的 shell dialect 编码结构化命令。
    fn shell_dialect(&self) -> RemoteShellDialect {
        self.shell_dialect
    }

    /// 只返回本连接认证后精确 nonce 应答的 shell 类型，不将 Posix 方言猜成 Bash。
    fn shell_kind(&self) -> RemoteShellKind {
        self.shell_kind
    }

    /// 建立独立 PTY channel；terminal pump 持有 peer Arc，关闭单个 PTY 不会断开父连接。
    async fn open_terminal(
        &self,
        request: TerminalOpenRequest,
        cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        self.ensure_open()?;
        let channel = self.open_session_channel(&cancel).await?;
        make_ssh_terminal_transport(
            channel,
            request,
            self.shell_dialect,
            self.shell_kind,
            self.operation_timeout,
            Arc::new(self.clone_for_channel()),
            cancel,
        )
        .await
    }

    /// 建立独立 exec channel，并通过有界事件队列分发 stdout、stderr 和真实退出状态。
    async fn exec_stream(
        &self,
        request: kerminal_application::connections::ExecRequest,
        cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        let command =
            encode_exec_command(&request, self.shell_dialect).map_err(exec_encoding_error)?;
        self.open_exec_stream(command, cancel).await
    }

    /// 原始脚本仅用于明确选择 shell 语义的请求，环境变量仍由可信 profile 方言安全编码。
    async fn exec_shell(
        &self,
        request: ShellScriptRequest,
        cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        let command =
            encode_shell_script(&request, self.shell_dialect).map_err(exec_encoding_error)?;
        self.open_exec_stream(command, cancel).await
    }

    /// 打开独立 SFTP subsystem channel；传输 stream 拥有该 channel，不关闭 lease 的其它 channel。
    async fn open_sftp(
        &self,
        _request: OpenSftpRequest,
        cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        let mut channel = self.open_session_channel(&cancel).await?;
        tokio::select! {
            _ = cancel.cancelled() => { close_channel_best_effort(&mut channel).await; return Err(AppError::cancelled("SSH SFTP 请求已取消")); }
            result = tokio::time::timeout(self.operation_timeout, channel.request_subsystem(true, "sftp")) => match result {
                Ok(Ok(())) => {},
                Ok(Err(error)) => { close_channel_best_effort(&mut channel).await; return Err(russh_error("SSH SFTP subsystem 请求发送失败", error)); },
                Err(_) => { close_channel_best_effort(&mut channel).await; return Err(AppError::timeout("SSH SFTP subsystem request send")); },
            },
        }
        let (channel, initial_data) =
            await_subsystem_reply(channel, &cancel, self.operation_timeout).await?;
        let stream = PrefixedPeerStream::new(
            Box::new(channel.into_stream()),
            initial_data,
            Arc::new(self.clone_for_channel()),
        );
        Ok(SftpChannelHandle::new(Box::new(stream)))
    }

    /// 打开一个独立 direct-tcpip channel 连接 SSH server 可达的目标 TCP endpoint。
    async fn open_tunnel(
        &self,
        request: TunnelRequest,
        cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        validate_tunnel_endpoint(&request.destination)?;
        let channel = {
            let handle = tokio::select! {
                _ = self.shutdown.cancelled() => return Err(AppError::cancelled("SSH lease is closing")),
                handle = self.target.handle.lock() => handle,
            };
            self.ensure_open()?;
            tokio::select! {
                _ = self.shutdown.cancelled() => return Err(AppError::cancelled("SSH lease is closing")),
                _ = cancel.cancelled() => return Err(AppError::cancelled("SSH tunnel 请求已取消")),
                result = tokio::time::timeout(self.operation_timeout, handle.channel_open_direct_tcpip(request.destination.host.clone(), u32::from(request.destination.port), "127.0.0.1", 0)) => match result {
                    Ok(Ok(channel)) => channel,
                    Ok(Err(error)) => return Err(russh_error("SSH direct-tcpip channel 打开失败", error)),
                    Err(_) => return Err(AppError::timeout("SSH tunnel channel")),
                },
            }
        };
        let stream = OwnedPeerStream::new(
            Box::new(channel.into_stream()),
            Arc::new(self.clone_for_channel()),
        );
        Ok(TunnelChannelHandle::new(Box::new(stream)))
    }

    /// 只有明确受理才发布 listener；请求结果未知时隔离旧端口或动态入口，不能让重试接收旧 channel。
    ///
    /// @author kongweiguang
    async fn request_remote_forward(
        &self,
        request: kerminal_application::connections::RemoteForwardRequest,
        cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        if request.bind_host.trim().is_empty() || request.bind_host.chars().any(char::is_control) {
            return Err(AppError::invalid(
                "SSH remote-forward bind address is invalid",
            ));
        }
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("SSH remote-forward 请求已取消"));
        }
        self.ensure_open()?;
        let (sender, receiver) = mpsc::channel(16);
        if request.bind_port == 0 {
            self.forward_router.register_pending(sender.clone())?;
        } else {
            self.forward_router
                .register(u32::from(request.bind_port), sender.clone())?;
        }
        let port = {
            let handle = tokio::select! {
                _ = self.shutdown.cancelled() => {
                    self.forward_router.remove_request(request.bind_port);
                    return Err(AppError::cancelled("SSH lease is closing"));
                }
                _ = cancel.cancelled() => {
                    self.forward_router.remove_request(request.bind_port);
                    return Err(AppError::cancelled("SSH remote-forward 请求已取消"));
                }
                handle = self.target.handle.lock() => handle,
            };
            if let Err(error) = self.ensure_open() {
                self.forward_router.remove_request(request.bind_port);
                return Err(error);
            }
            let result = tokio::select! {
                _ = self.shutdown.cancelled() => Err(RemoteForwardRequestFailure::Unknown(AppError::cancelled("SSH lease is closing"))),
                _ = cancel.cancelled() => Err(RemoteForwardRequestFailure::Unknown(AppError::cancelled("SSH remote-forward 请求已取消"))),
                result = tokio::time::timeout(self.operation_timeout, handle.tcpip_forward(request.bind_host.clone(), u32::from(request.bind_port))) => match result {
                    Ok(Ok(port)) => Ok(port),
                    Ok(Err(error)) => {
                        let definitive = is_definitive_remote_forward_request_failure(&error);
                        let error = russh_error("SSH remote-forward 请求失败", error);
                        Err(if definitive {
                            RemoteForwardRequestFailure::Definitive(error)
                        } else {
                            RemoteForwardRequestFailure::Unknown(error)
                        })
                    },
                    Err(_) => Err(RemoteForwardRequestFailure::Unknown(AppError::timeout("SSH remote-forward"))),
                },
            };
            match result {
                Ok(port) => port,
                Err(RemoteForwardRequestFailure::Definitive(error)) => {
                    self.forward_router.remove_request(request.bind_port);
                    return Err(error);
                }
                Err(RemoteForwardRequestFailure::Unknown(error)) => {
                    drop(handle);
                    self.isolate_unknown_remote_forward_setup(
                        &request.bind_host,
                        request.bind_port,
                    )
                    .await;
                    return Err(error);
                }
            }
        };
        let bind_port = match effective_remote_forward_port(request.bind_port, port) {
            Ok(bind_port) => bind_port,
            Err(error) => {
                self.isolate_unknown_remote_forward_setup(&request.bind_host, request.bind_port)
                    .await;
                return Err(error);
            }
        };
        if request.bind_port == 0
            && let Err(error) = self
                .forward_router
                .promote_pending(u32::from(bind_port), sender)
        {
            self.cancel_remote_forward_best_effort(request.bind_host.clone(), u32::from(bind_port))
                .await;
            return Err(error);
        }
        Ok(RemoteForwardHandle::new(Box::new(
            RusshRemoteForwardListener {
                session: Arc::clone(&self.target),
                router: Arc::clone(&self.forward_router),
                receiver,
                bind_host: request.bind_host,
                bind_port,
                closed: false,
                peer: Arc::new(self.clone_for_channel()),
                operation_timeout: self.operation_timeout,
            },
        )))
    }

    /// 只在 lease owner 显式关闭时断开所有 jump hop 与 target 连接。
    async fn close_until(&self, deadline: tokio::time::Instant, reason: &str) -> AppResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.closing.store(true, Ordering::Release);
        self.shutdown.cancel();
        let now = tokio::time::Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            self.cancel_session_transports();
            return Err(AppError::timeout("SSH lease close deadline"));
        }
        let force_close_budget = (remaining / 2).min(Duration::from_millis(500));
        let graceful_deadline = deadline - force_close_budget;
        let _close_guard =
            match tokio::time::timeout_at(graceful_deadline, self.close_lock.lock()).await {
                Ok(guard) => guard,
                Err(_) => {
                    self.cancel_session_transports();
                    return Err(AppError::timeout("SSH lease close lock"));
                }
            };
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.forward_router.clear();
        let mut first_error = None;
        for session in self.sessions.iter().rev() {
            if session.disconnected.load(Ordering::Acquire) {
                continue;
            }
            if let Err(error) =
                disconnect_one_session_until(session, reason, graceful_deadline, deadline).await
            {
                first_error.get_or_insert(error);
            }
        }
        if self
            .sessions
            .iter()
            .all(|session| session.disconnected.load(Ordering::Acquire))
        {
            self.closed.store(true, Ordering::Release);
            self.lifetime_owner.mark_closed();
        }
        if !self.closed.load(Ordering::Acquire) && tokio::time::Instant::now() >= deadline {
            self.cancel_session_transports();
        }
        if let Some(error) = first_error {
            Err(error)
        } else if self.closed.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(AppError::timeout("SSH lease task shutdown"))
        }
    }
}

impl RusshPeer {
    /// 关闭期限耗尽时同步取消所有底层 transport token，但保留 Handle 供后续 close_until join。
    fn cancel_session_transports(&self) {
        for session in &self.sessions {
            session.transport_cancel.cancel();
        }
    }

    /// 克隆句柄时共享 sessions 与转发路由，使 channel 不依赖调用栈中的借用寿命。
    fn clone_for_channel(&self) -> Self {
        Self {
            host_id: self.host_id.clone(),
            capabilities: self.capabilities,
            shell_dialect: self.shell_dialect,
            shell_kind: self.shell_kind,
            operation_timeout: self.operation_timeout,
            target: Arc::clone(&self.target),
            sessions: self.sessions.clone(),
            forward_router: Arc::clone(&self.forward_router),
            closing: Arc::clone(&self.closing),
            closed: Arc::clone(&self.closed),
            close_lock: Arc::clone(&self.close_lock),
            shutdown: self.shutdown.clone(),
            lifetime_owner: Arc::clone(&self.lifetime_owner),
        }
    }

    /// 打开并确认一次独立 exec channel，再把双方向事件桥接到有界应用流。
    async fn open_exec_stream(
        &self,
        command: String,
        cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        validate_tunnel_target(&self.target.endpoint)?;
        let mut channel = self.open_session_channel(&cancel).await?;
        tokio::select! {
            _ = cancel.cancelled() => { close_channel_best_effort(&mut channel).await; return Err(AppError::cancelled("SSH exec 请求已取消")); }
            result = tokio::time::timeout(self.operation_timeout, channel.exec(true, command.into_bytes())) => match result {
                Ok(Ok(())) => {},
                Ok(Err(error)) => { close_channel_best_effort(&mut channel).await; return Err(russh_error("SSH exec 请求发送失败", error)); },
                Err(_) => { close_channel_best_effort(&mut channel).await; return Err(AppError::timeout("SSH exec request send")); },
            },
        }
        let (channel, initial_events) =
            await_session_request_reply(channel, &cancel, self.operation_timeout).await?;
        let (reader, writer) = channel.split();
        // 子模块只接收准入成功的 channel 半部；child token 让调用方取消能关闭本 channel。
        let pump_cancel = cancel.child_token();
        Ok(command_stream::from_channel(
            reader,
            writer,
            Arc::new(self.clone_for_channel()),
            pump_cancel,
            initial_events,
        ))
    }

    /// 为每项功能创建独立 session channel，串行化的只是短暂 channel-open 请求。
    async fn open_session_channel(
        &self,
        cancel: &CancellationToken,
    ) -> AppResult<russh::Channel<Msg>> {
        let handle = tokio::select! {
            _ = self.shutdown.cancelled() => return Err(AppError::cancelled("SSH lease is closing")),
            _ = cancel.cancelled() => return Err(AppError::cancelled("SSH session channel 请求已取消")),
            handle = self.target.handle.lock() => handle,
        };
        self.ensure_open()?;
        tokio::select! {
            _ = self.shutdown.cancelled() => Err(AppError::cancelled("SSH lease is closing")),
            _ = cancel.cancelled() => Err(AppError::cancelled("SSH session channel 请求已取消")),
            result = tokio::time::timeout(self.operation_timeout, handle.channel_open_session()) => match result {
                Ok(Ok(channel)) => Ok(channel),
                Ok(Err(error)) => Err(russh_error("SSH session channel 打开失败", error)),
                Err(_) => Err(AppError::timeout("SSH session channel")),
            },
        }
    }

    /// 拒绝在 lease 已显式关闭后创建新 channel，避免对失效 session 做隐式重连。
    fn ensure_open(&self) -> AppResult<()> {
        if self.closing.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            Err(AppError::conflict("SSH connection lease is closed"))
        } else {
            Ok(())
        }
    }

    /// 结果未知时继续由当前 peer 持有回收责任，只撤销可信的固定请求端口，不使用异常回执猜测端口。
    ///
    /// @author kongweiguang
    async fn isolate_unknown_remote_forward_setup(&self, address: &str, requested_port: u16) {
        self.forward_router.retain_unknown_request(requested_port);
        if requested_port != 0
            && self
                .cancel_remote_forward_best_effort(address.to_owned(), u32::from(requested_port))
                .await
        {
            self.forward_router.confirm_cancel(requested_port);
        }
    }

    /// 撤销尝试有界，只有真实成功 ACK 返回 true；调用方始终保留原始失败，不把清理当启动成功。
    ///
    /// @author kongweiguang
    async fn cancel_remote_forward_best_effort(&self, address: String, port: u32) -> bool {
        let result = tokio::time::timeout(CLOSE_OPERATION_TIMEOUT, async {
            self.target
                .handle
                .lock()
                .await
                .cancel_tcpip_forward(address, port)
                .await
        })
        .await;
        matches!(result, Ok(Ok(())))
    }
}

/// 区分尚未入队和结果未知，防止把取消、超时或无法辨明的回执误当作远端没有创建 listener。
enum RemoteForwardRequestFailure {
    Definitive(AppError),
    Unknown(AppError),
}

/// 所有路由变更共用一把锁，避免端口退休与动态 pending 提升之间出现错误接收窗口。
#[derive(Default)]
struct ForwardRouter {
    state: StdMutex<ForwardRouterState>,
}

/// retired 只接受非零 u16 端口，每个 peer 最多 65535 项；未知取消结果不能因容量策略被提前遗忘。
#[derive(Default)]
struct ForwardRouterState {
    senders: HashMap<u16, mpsc::Sender<russh::Channel<Msg>>>,
    retired: HashSet<u16>,
    pending: Option<mpsc::Sender<russh::Channel<Msg>>>,
    pending_blocked: bool,
}

impl ForwardRouter {
    /// 拒绝重复与取消结果未知的端口，防止新 listener 抢占旧 listener 的迟到 channel。
    ///
    /// @author kongweiguang
    fn register(&self, port: u32, sender: mpsc::Sender<russh::Channel<Msg>>) -> AppResult<()> {
        let port = checked_remote_forward_route_port(port)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.senders.contains_key(&port) || state.retired.contains(&port) {
            return Err(AppError::conflict(
                "SSH remote-forward port is already registered",
            ));
        }
        state.senders.insert(port, sender);
        Ok(())
    }

    /// 动态分配端口在服务器答复前不可知，因此只允许一个未完成的动态请求。
    ///
    /// @author kongweiguang
    fn register_pending(&self, sender: mpsc::Sender<russh::Channel<Msg>>) -> AppResult<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.pending_blocked {
            return Err(AppError::conflict(UNKNOWN_DYNAMIC_REMOTE_FORWARD_MESSAGE));
        }
        if state.pending.is_some() {
            return Err(AppError::conflict(
                "SSH dynamic remote-forward request is already pending",
            ));
        }
        state.pending = Some(sender);
        Ok(())
    }

    /// 在同一临界区迁移 pending；冲突也释放本次 pending，但不能覆盖尚未确认取消的旧端口。
    ///
    /// @author kongweiguang
    fn promote_pending(
        &self,
        port: u32,
        sender: mpsc::Sender<russh::Channel<Msg>>,
    ) -> AppResult<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.pending = None;
        let port = checked_remote_forward_route_port(port)?;
        if state.pending_blocked {
            return Err(AppError::conflict(UNKNOWN_DYNAMIC_REMOTE_FORWARD_MESSAGE));
        }
        if state.senders.contains_key(&port) || state.retired.contains(&port) {
            return Err(AppError::conflict(
                "SSH remote-forward port is already registered",
            ));
        }
        state.senders.insert(port, sender);
        Ok(())
    }

    /// 在发送 cancel 前原子屏蔽旧端口；ACK 前的迟到 channel 不能落入无关动态 pending。
    ///
    /// @author kongweiguang
    fn retire(&self, port: u32) -> AppResult<()> {
        let port = checked_remote_forward_route_port(port)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.senders.remove(&port);
        state.retired.insert(port);
        Ok(())
    }

    /// 同一 peer 的接收循环已处理成功取消 ACK 后才允许复用；失败与超时保留隔离状态供重试。
    ///
    /// @author kongweiguang
    fn confirm_cancel(&self, port: u16) {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retired
            .remove(&port);
    }

    /// 请求已明确失败时只释放本次申请；不能清除先前尚未知晓结果的退休端口或动态阻塞。
    ///
    /// @author kongweiguang
    fn remove_request(&self, requested_port: u16) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if requested_port == 0 {
            state.pending = None;
        } else {
            state.senders.remove(&requested_port);
        }
    }

    /// 未知固定结果保留端口隔离，未知动态结果锁住新动态申请；peer owner 继续负责最终断开回收。
    ///
    /// @author kongweiguang
    fn retain_unknown_request(&self, requested_port: u16) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if requested_port == 0 {
            state.pending = None;
            state.pending_blocked = true;
        } else {
            state.senders.remove(&requested_port);
            state.retired.insert(requested_port);
        }
    }

    /// lease 关闭时不再接纳 remote-forward channel。
    ///
    /// @author kongweiguang
    fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.senders.clear();
        state.retired.clear();
        state.pending = None;
        state.pending_blocked = false;
    }

    /// 优先精确路由，退休端口拒绝回退；其它合法未知端口仍覆盖动态 success→promote 的短暂窗口。
    ///
    /// @author kongweiguang
    fn sender(&self, port: u32) -> Option<mpsc::Sender<russh::Channel<Msg>>> {
        let port = checked_remote_forward_route_port(port).ok()?;
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(sender) = state.senders.get(&port) {
            return Some(sender.clone());
        }
        if state.retired.contains(&port) || state.pending_blocked {
            return None;
        }
        state.pending.clone()
    }
}

/// russh 0.62.5 将明确拒绝与畸形成功回执都映射为 RequestDenied，只有 SendError 能证明未入队。
///
/// @author kongweiguang
fn is_definitive_remote_forward_request_failure(error: &russh::Error) -> bool {
    matches!(error, russh::Error::SendError)
}

/// 路由身份限制为有效 TCP 端口，既拒绝异常协议值，也给 peer 内退休集合提供固定上限。
///
/// @author kongweiguang
fn checked_remote_forward_route_port(port: u32) -> AppResult<u16> {
    u16::try_from(port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| AppError::invalid("SSH remote-forward route port is invalid"))
}

/// 将 Russh/RFC 的回执转换为 listener 端口；固定请求的空回执表示原请求端口已绑定。
fn effective_remote_forward_port(requested_port: u16, returned_port: u32) -> AppResult<u16> {
    let effective_port = if requested_port == 0 {
        returned_port
    } else if returned_port == 0 || returned_port == u32::from(requested_port) {
        u32::from(requested_port)
    } else {
        return Err(AppError::infrastructure(
            "SSH server returned a different remote-forward port",
        ));
    };
    if effective_port == 0 || effective_port > u32::from(u16::MAX) {
        return Err(AppError::infrastructure(
            "SSH server returned an invalid remote-forward port",
        ));
    }
    u16::try_from(effective_port)
        .map_err(|_| AppError::infrastructure("SSH server returned an invalid remote-forward port"))
}

/// Russh handler 自动信任主机密钥并尽力记录 known_hosts，同时接收远端端口转发的入站 channel。
struct RusshHandler {
    endpoint: HostEndpoint,
    known_hosts: Arc<dyn KnownHostsRepository>,
    decision_error: Arc<StdMutex<Option<AppError>>>,
    forward_router: Arc<ForwardRouter>,
}

impl client::Handler for RusshHandler {
    type Error = russh::Error;

    /// 默认信任所有主机密钥：观察到 Unknown/Changed 时尽力写入 known_hosts，随后一律放行。
    ///
    /// 主机密钥已不再承担连接准入职责，known_hosts 只用于迁移和排障；因此读取或写入失败
    /// 都降级为 warning，不能阻断握手或认证。只有公钥无法序列化这类内部不变量错误，
    /// 因为无法形成可持久化的 key 身份，才继续拒绝握手。
    async fn check_server_key(
        &mut self,
        key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
        let algorithm = key.algorithm().to_string();
        let public_key_openssh = match key.to_openssh() {
            Ok(public_key) => public_key,
            Err(error) => {
                *self
                    .decision_error
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) =
                    Some(AppError::infrastructure_with_source(
                        "SSH host key serialization failed",
                        error,
                    ));
                return Ok(false);
            }
        };
        let identity = HostKeyIdentity {
            algorithm,
            fingerprint_sha256: fingerprint,
            public_key_openssh,
        };
        match self.known_hosts.inspect(&self.endpoint, &identity).await {
            Ok(status) => {
                if status != HostKeyStatus::Known {
                    // 自动信任后写入只是后台记录；失败不能改变握手结果，也不能留下
                    // decision_error 把后续 KEX/认证错误误分类成主机密钥错误。
                    if let Err(error) = self.known_hosts.record(&self.endpoint, &identity).await {
                        tracing::warn!(
                            error = %error,
                            "SSH known_hosts 自动记录主机密钥失败；连接继续"
                        );
                    }
                }
                Ok(true)
            }
            Err(error) => {
                // 读取失败同样只是记录能力降级；普通连接必须照常完成握手。
                tracing::warn!(
                    error = %error,
                    "SSH known_hosts 读取主机密钥状态失败；连接继续"
                );
                Ok(true)
            }
        }
    }

    /// 接收精确路由或动态回执窗口的 channel；退休端口、非法端口与满队列均显式拒绝。
    ///
    /// @author kongweiguang
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        if let Some(sender) = self.forward_router.sender(connected_port) {
            match sender.try_reserve() {
                Ok(permit) => {
                    reply.accept().await;
                    permit.send(channel);
                }
                Err(_) => {
                    reply
                        .reject(russh::ChannelOpenFailure::ResourceShortage)
                        .await;
                }
            }
        } else {
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
        }
        Ok(())
    }
}

/// 每个 route hop 记录 endpoint 和认证材料；主机密钥已由 handler 自动信任，无需 peer 编号。
#[derive(Clone)]
struct PeerSpec {
    endpoint: HostEndpoint,
    authentication: Option<SshAuthentication>,
}

/// 同一 peer handshake 共享的转发路由器与清理所有者；保留 Arc 身份以确保关闭仍落到原 owner。
///
/// @author kongweiguang
struct PeerConnectionContext {
    forward_router: Arc<ForwardRouter>,
    pending_cleanup: Arc<PendingRusshCleanup>,
    reservation: Arc<RusshCleanupReservation>,
}

/// 按 proxy → jump hosts → target 顺序建立完整认证链。
///
/// @author kongweiguang
async fn connect_route(
    profile: &HostConnectionProfile,
    dependencies: &RusshRemoteConnectionDependencies,
    forward_router: Arc<ForwardRouter>,
    pending_cleanup: Arc<PendingRusshCleanup>,
    reservation: Arc<RusshCleanupReservation>,
    cancel: &CancellationToken,
) -> AppResult<Vec<Arc<RusshPeerSession>>> {
    let route = peer_specs(profile)?;
    let first = route
        .first()
        .ok_or_else(|| AppError::invalid("SSH route has no peers"))?;
    let stream: Box<dyn AsyncDuplex> = if let Some(proxy) = &profile.route.proxy {
        tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("SSH proxy route cancelled")),
            result = tokio::time::timeout(connect_timeout(profile.connect_timeout_ms), connect_via_proxy(proxy, &first.endpoint, dependencies.credentials.as_ref(), cancel)) => match result {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(AppError::timeout("SSH proxy handshake")),
            }
        }
    } else {
        connect_tcp(&first.endpoint, profile.connect_timeout_ms, cancel).await?
    };
    let mut sessions = SessionSetGuard::new(Arc::clone(&pending_cleanup), Arc::clone(&reservation));
    let mut next_stream = Some(stream);
    let peer_count = route.len();
    for (index, spec) in route.into_iter().enumerate() {
        if cancel.is_cancelled() {
            let _ = disconnect_sessions(
                sessions.sessions(),
                &pending_cleanup,
                &reservation,
                "Kerminal SSH route cancelled",
            )
            .await;
            return Err(AppError::cancelled("SSH connection route cancelled"));
        }
        let stream = next_stream
            .take()
            .ok_or_else(|| AppError::infrastructure("SSH route stream was not initialized"))?;
        let authentication = spec.authentication.clone();
        let session = match connect_peer(
            profile,
            dependencies,
            spec,
            stream,
            PeerConnectionContext {
                forward_router: Arc::clone(&forward_router),
                pending_cleanup: Arc::clone(&pending_cleanup),
                reservation: Arc::clone(&reservation),
            },
            cancel,
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                let _ = disconnect_sessions(
                    sessions.sessions(),
                    &pending_cleanup,
                    &reservation,
                    "Kerminal SSH route setup failed",
                )
                .await;
                return Err(error);
            }
        };
        sessions.push(Arc::clone(&session));
        let Some(authentication) = authentication.as_ref() else {
            let _ = disconnect_sessions(
                sessions.sessions(),
                &pending_cleanup,
                &reservation,
                "Kerminal SSH peer has no authentication",
            )
            .await;
            return Err(AppError::invalid("SSH peer has no authentication method"));
        };
        if let Err(error) = authenticate_peer(
            &session,
            authentication,
            dependencies.credentials.as_ref(),
            cancel,
            profile.connect_timeout_ms,
        )
        .await
        {
            let _ = disconnect_sessions(
                sessions.sessions(),
                &pending_cleanup,
                &reservation,
                "Kerminal SSH peer authentication failed",
            )
            .await;
            return Err(error);
        }
        if index + 1 < peer_count {
            let next = peer_specs(profile)?
                .get(index + 1)
                .cloned()
                .ok_or_else(|| AppError::infrastructure("SSH route hop is missing"))?;
            let handle = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = disconnect_sessions(
                        sessions.sessions(),
                        &pending_cleanup,
                        &reservation,
                        "Kerminal SSH jump setup cancelled",
                    )
                    .await;
                    return Err(AppError::cancelled("SSH jump connection cancelled"));
                }
                handle = session.handle.lock() => handle,
            };
            let open_result = tokio::select! {
                _ = cancel.cancelled() => Err(AppError::cancelled("SSH jump connection cancelled")),
                result = tokio::time::timeout(connect_timeout(profile.connect_timeout_ms), handle.channel_open_direct_tcpip(next.endpoint.host.clone(), u32::from(next.endpoint.port), "127.0.0.1", 0)) => match result {
                    Ok(Ok(channel)) => Ok(channel),
                    Ok(Err(error)) => Err(russh_error("SSH jump channel open failed", error)),
                    Err(_) => Err(AppError::timeout("SSH jump channel")),
                }
            };
            drop(handle);
            let channel = match open_result {
                Ok(channel) => channel,
                Err(error) => {
                    let _ = disconnect_sessions(
                        sessions.sessions(),
                        &pending_cleanup,
                        &reservation,
                        "Kerminal SSH jump channel failed",
                    )
                    .await;
                    return Err(error);
                }
            };
            next_stream = Some(Box::new(channel.into_stream()));
        }
    }
    Ok(sessions.into_sessions())
}

/// 完成一个 SSH peer 的加密 handshake；主机密钥由 handler 自动信任，凭据认证在调用方按 route 层次追加。
/// 将同一 handshake 的清理资源打包，避免丢失任何回收 owner。
///
/// @author kongweiguang
async fn connect_peer(
    profile: &HostConnectionProfile,
    dependencies: &RusshRemoteConnectionDependencies,
    spec: PeerSpec,
    stream: Box<dyn AsyncDuplex>,
    context: PeerConnectionContext,
    cancel: &CancellationToken,
) -> AppResult<Arc<RusshPeerSession>> {
    let PeerConnectionContext {
        forward_router,
        pending_cleanup,
        reservation,
    } = context;
    let transport_cancel = CancellationToken::new();
    let stream: Box<dyn AsyncDuplex> =
        Box::new(CancellablePeerStream::new(stream, transport_cancel.clone()));
    let decision_error = Arc::new(StdMutex::new(None));
    let handler = RusshHandler {
        endpoint: spec.endpoint.clone(),
        known_hosts: Arc::clone(&dependencies.known_hosts),
        decision_error: Arc::clone(&decision_error),
        forward_router,
    };
    let config = russh::client::Config {
        nodelay: true,
        keepalive_interval: profile.keepalive_interval_ms.map(Duration::from_millis),
        ..Default::default()
    };
    let timeout = connect_timeout(profile.connect_timeout_ms);
    let task = tokio::spawn(client::connect_stream(Arc::new(config), stream, handler));
    let handshake = PendingHandshakeCleanup::new(
        Arc::clone(&reservation),
        spec.endpoint.clone(),
        Arc::clone(&decision_error),
        transport_cancel.clone(),
        task,
    );
    let mut registration =
        HandshakeRegistrationGuard::new(Arc::clone(&pending_cleanup), Arc::clone(&handshake));
    let handshake_deadline = tokio::time::Instant::now() + timeout;
    let connected = tokio::select! {
        _ = cancel.cancelled() => {
            handshake.cancel();
            registration.retain();
            return Err(AppError::cancelled("SSH handshake cancelled"));
        }
        result = handshake.session_until(handshake_deadline) => result,
    };
    let session = match connected {
        Ok(Some(session)) => {
            registration.disarm();
            session
        }
        Ok(None) => {
            registration.disarm();
            return Err(AppError::infrastructure(
                "SSH handshake completed without a session",
            ));
        }
        Err(error) => {
            if handshake.is_complete() {
                registration.disarm();
            } else {
                handshake.cancel();
                registration.retain();
            }
            return Err(error);
        }
    };
    Ok(session)
}

/// 统一创建 handle owner，使 handshake 取消和底层 transport 取消也能显式回收 russh task。
fn make_peer_session(
    handle: Handle<RusshHandler>,
    endpoint: &HostEndpoint,
    transport_cancel: CancellationToken,
) -> Arc<RusshPeerSession> {
    Arc::new(RusshPeerSession {
        handle: Mutex::new(handle),
        endpoint: endpoint.clone(),
        disconnected: AtomicBool::new(false),
        disconnect_requested: AtomicBool::new(false),
        transport_cancel,
    })
}

/// 仅在密钥可信后解析一次凭据，并按 host profile 认证当前 route hop。
async fn authenticate_peer(
    session: &Arc<RusshPeerSession>,
    authentication: &SshAuthentication,
    credentials: &dyn CredentialResolver,
    cancel: &CancellationToken,
    timeout_ms: u64,
) -> AppResult<()> {
    let username = session.endpoint.username.clone();
    let result = match authentication {
        SshAuthentication::Password { secret } => {
            let credential = credentials.resolve(secret, cancel).await?;
            let ResolvedCredential::Password(password) = credential else {
                return Err(AppError::invalid(
                    "SSH password credential has the wrong type",
                ));
            };
            let mut handle = session.handle.lock().await;
            tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled("SSH authentication cancelled")),
                result = tokio::time::timeout(connect_timeout(timeout_ms), handle.authenticate_password(username, password.as_str().to_owned())) => match result {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => return Err(russh_error("SSH password authentication failed", error)),
                    Err(_) => return Err(AppError::timeout("SSH authentication")),
                }
            }
        }
        SshAuthentication::PrivateKey {
            path,
            secret,
            passphrase,
        } => {
            let key_bytes = match secret {
                Some(secret) => match credentials.resolve(secret, cancel).await? {
                    ResolvedCredential::PrivateKey(bytes) => bytes,
                    _ => {
                        return Err(AppError::invalid(
                            "SSH private-key credential has the wrong type",
                        ));
                    }
                },
                None => {
                    let path = path
                        .as_ref()
                        .ok_or_else(|| AppError::invalid("SSH private-key source is missing"))?;
                    let bytes = tokio::select! {
                        _ = cancel.cancelled() => return Err(AppError::cancelled("SSH private-key loading cancelled")),
                        result = tokio::time::timeout(connect_timeout(timeout_ms), async {
                            let file = tokio::fs::File::open(path).await?;
                            let mut bytes = Vec::with_capacity(8192);
                            file.take((MAX_PRIVATE_KEY_BYTES + 1) as u64).read_to_end(&mut bytes).await?;
                            Ok::<_, std::io::Error>(bytes)
                        }) => match result {
                            Ok(Ok(bytes)) if bytes.len() <= MAX_PRIVATE_KEY_BYTES => bytes,
                            Ok(Ok(_)) => return Err(AppError::invalid("SSH private key exceeds the supported size")),
                            Ok(Err(error)) => return Err(AppError::infrastructure_with_source("SSH private-key file could not be read", error)),
                            Err(_) => return Err(AppError::timeout("SSH private-key loading")),
                        }
                    };
                    Zeroizing::new(bytes)
                }
            };
            let passphrase = if let Some(reference) = passphrase {
                match credentials.resolve(reference, cancel).await? {
                    ResolvedCredential::KeyPassphrase(value) => Some(value),
                    _ => {
                        return Err(AppError::invalid(
                            "SSH key passphrase credential has the wrong type",
                        ));
                    }
                }
            } else {
                None
            };
            if key_bytes.len() > MAX_PRIVATE_KEY_BYTES {
                return Err(AppError::invalid(
                    "SSH private key exceeds the supported size",
                ));
            }
            let key_decode = tokio::task::spawn_blocking(move || {
                let key_text = std::str::from_utf8(&key_bytes).map_err(|error| {
                    AppError::infrastructure_with_source(
                        "SSH private-key encoding is invalid",
                        error,
                    )
                })?;
                russh::keys::decode_secret_key(
                    key_text,
                    passphrase.as_ref().map(|value| value.as_str()),
                )
                .map_err(|error| {
                    AppError::infrastructure_with_source(
                        "SSH private key could not be decoded",
                        error,
                    )
                })
            });
            let key = tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled("SSH private-key decoding cancelled")),
                result = tokio::time::timeout(connect_timeout(timeout_ms), key_decode) => match result {
                    Ok(Ok(Ok(key))) => key,
                    Ok(Ok(Err(error))) => return Err(error),
                    Ok(Err(error)) => return Err(AppError::infrastructure_with_source("SSH private-key worker failed", error)),
                    Err(_) => return Err(AppError::timeout("SSH private-key decoding")),
                }
            };
            // RSA 服务器常同时公布 rsa-sha2-* 并拒绝旧 ssh-rsa；必须按当前 peer 的
            // server-sig-algs 选择哈希，否则同一 id_rsa 在 OpenSSH 可用而这里认证失败。
            // 协商与签名共用一次认证超时，避免扩展信息等待另占一份完整预算。
            let mut handle = session.handle.lock().await;
            tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled("SSH authentication cancelled")),
                result = tokio::time::timeout(connect_timeout(timeout_ms), async {
                    let rsa_hash = if key.algorithm().is_rsa() {
                        negotiated_rsa_hash(&handle).await?
                    } else {
                        None
                    };
                    let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
                    handle.authenticate_publickey(username, key).await
                        .map_err(|error| russh_error("SSH public-key authentication failed", error))
                }) => match result {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Err(AppError::timeout("SSH authentication")),
                }
            }
        }
        SshAuthentication::Agent => {
            return authenticate_agent(session, cancel, timeout_ms).await;
        }
    };
    if result == AuthResult::Success {
        Ok(())
    } else {
        Err(AppError::authentication_failed())
    }
}

/// 使用系统 SSH agent 逐个尝试身份；不可用的认证提供方映射为稳定认证错误，保留脱敏诊断源。
/// 私钥不离开 agent，Windows 优先连接 OpenSSH named pipe。
///
/// @author kongweiguang
async fn authenticate_agent(
    session: &Arc<RusshPeerSession>,
    cancel: &CancellationToken,
    timeout_ms: u64,
) -> AppResult<()> {
    #[cfg(windows)]
    let mut agent = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SSH agent connection cancelled")),
        result = tokio::time::timeout(connect_timeout(timeout_ms), AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")) => match result {
            Ok(Ok(agent)) => agent,
            Ok(Err(error)) => return Err(unavailable_ssh_agent(error)),
            Err(_) => return Err(AppError::timeout("SSH agent connection")),
        }
    };
    #[cfg(unix)]
    let mut agent = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SSH agent connection cancelled")),
        result = tokio::time::timeout(connect_timeout(timeout_ms), AgentClient::connect_env()) => match result {
            Ok(Ok(agent)) => agent,
            Ok(Err(error)) => return Err(unavailable_ssh_agent(error)),
            Err(_) => return Err(AppError::timeout("SSH agent connection")),
        }
    };
    #[cfg(not(any(unix, windows)))]
    return Err(AppError::capability_not_supported(
        "SSH agent on this platform",
    ));

    #[cfg(any(unix, windows))]
    {
        authenticate_agent_identities(session, &mut agent, cancel, timeout_ms).await
    }
}

/// 逐个尝试 agent 身份。RSA 身份按当前 peer 公布的 server-sig-algs 选择 SHA-2，非 RSA
/// 身份继续传 `None`；协商结果在同一 peer 内缓存，避免每个身份重复等待扩展信息。
///
/// @author kongweiguang
async fn authenticate_agent_identities<S>(
    session: &Arc<RusshPeerSession>,
    agent: &mut AgentClient<S>,
    cancel: &CancellationToken,
    timeout_ms: u64,
) -> AppResult<()>
where
    S: AgentStream + Send + Unpin,
{
    let identities = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SSH agent identity request cancelled")),
        result = tokio::time::timeout(connect_timeout(timeout_ms), agent.request_identities()) => match result {
            Ok(Ok(identities)) => identities,
            Ok(Err(error)) => return Err(unavailable_ssh_agent(error)),
            Err(_) => return Err(AppError::timeout("SSH agent identity request")),
        }
    };
    let mut handle = session.handle.lock().await;
    // 外层 None 表示尚未查询；Some(None) 表示服务端明确未公布 SHA-2，应沿用旧签名。
    let mut rsa_hash: Option<Option<HashAlg>> = None;
    for identity in identities {
        let public_key = identity.public_key().into_owned();
        let is_rsa = public_key.algorithm().is_rsa();
        let cached_rsa_hash = rsa_hash;
        let (result, used_rsa_hash) = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("SSH agent authentication cancelled")),
            result = tokio::time::timeout(connect_timeout(timeout_ms), async {
                // RSA 身份的协商与本次签名共用同一认证超时，避免每个身份额外增加一份等待预算。
                let hash_alg = if is_rsa {
                    match cached_rsa_hash {
                        Some(hash) => hash,
                        None => negotiated_rsa_hash(&handle).await?,
                    }
                } else {
                    None
                };
                let result = handle
                    .authenticate_publickey_with(session.endpoint.username.clone(), public_key, hash_alg, agent)
                    .await
                    .map_err(|error| AppError::infrastructure_with_source("SSH agent signing failed", error))?;
                Ok::<_, AppError>((result, hash_alg))
            }) => match result {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(AppError::timeout("SSH agent authentication")),
            }
        };
        // 只缓存 RSA 身份的结果；非 RSA 的 None 不覆盖“尚未查询”状态。
        if is_rsa {
            rsa_hash = Some(used_rsa_hash);
        }
        if result == AuthResult::Success {
            return Ok(());
        }
    }
    Err(AppError::authentication_failed())
}

/// 查询当前 peer 的 RSA 签名算法。服务端未支持扩展或没有公布 SHA-2 时返回 `None`，
/// 沿用 russh 原有的旧 ssh-rsa 签名以保持老服务器兼容；认证被拒后不静默降级重试，
/// 避免把真实的授权失败掩盖成算法回退。调用方把本调用放在既有认证超时与取消范围内，
/// 因此这里不再单独叠加超时；russh 内部等待扩展信息最多 1 秒。
///
/// @author kongweiguang
async fn negotiated_rsa_hash(handle: &Handle<RusshHandler>) -> AppResult<Option<HashAlg>> {
    handle
        .best_supported_rsa_hash()
        .await
        .map(|rsa_hash| rsa_hash.flatten())
        .map_err(|error| russh_error("SSH RSA signature negotiation failed", error))
}

/// 将认证提供方不可用转为稳定认证分类，底层原因仅留在受控的诊断链中。
///
/// @author kongweiguang
fn unavailable_ssh_agent(error: impl std::error::Error + Send + Sync + 'static) -> AppError {
    AppError::authentication_failed().with_source(error)
}

/// 使用 Tokio TCP stream 连接最外层 endpoint，超时与用户取消均可中止网络拨号。
async fn connect_tcp(
    endpoint: &HostEndpoint,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> AppResult<Box<dyn AsyncDuplex>> {
    validate_endpoint(endpoint)?;
    let stream = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SSH TCP connection cancelled")),
        result = tokio::time::timeout(connect_timeout(timeout_ms), tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))) => match result {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return Err(AppError::infrastructure_with_source("SSH TCP connection failed", error)),
            Err(_) => return Err(AppError::timeout("SSH TCP connection")),
        }
    };
    stream.set_nodelay(true).map_err(|error| {
        AppError::infrastructure_with_source("SSH TCP socket setup failed", error)
    })?;
    Ok(Box::new(stream))
}

/// 解析 route 中每个 SSH peer 的身份和认证引用，跳板序号沿用 profile 中的稳定顺序。
fn peer_specs(profile: &HostConnectionProfile) -> AppResult<Vec<PeerSpec>> {
    if profile.route.jump_hosts.len() > MAX_JUMP_HOSTS {
        return Err(AppError::invalid("SSH route exceeds the jump-host limit"));
    }
    let mut peers = profile
        .route
        .jump_hosts
        .iter()
        .map(|jump| PeerSpec {
            endpoint: jump.endpoint.clone(),
            authentication: Some(jump.authentication.clone()),
        })
        .collect::<Vec<_>>();
    peers.push(PeerSpec {
        endpoint: profile.endpoint.clone(),
        authentication: profile.authentication.clone(),
    });
    for peer in &peers {
        validate_endpoint(&peer.endpoint)?;
    }
    Ok(peers)
}

/// 对已建立的连接链发送显式 disconnect；失败的 session 立即转给服务级 owner 重试。
async fn disconnect_sessions(
    sessions: &[Arc<RusshPeerSession>],
    pending_cleanup: &PendingRusshCleanup,
    reservation: &Arc<RusshCleanupReservation>,
    reason: &str,
) -> AppResult<()> {
    let deadline = tokio::time::Instant::now() + CLOSE_OPERATION_TIMEOUT;
    let graceful_deadline = deadline
        .checked_sub(Duration::from_millis(500))
        .unwrap_or(deadline);
    let mut first_error = None;
    for session in sessions.iter().rev() {
        if let Err(error) =
            disconnect_one_session_until(session, reason, graceful_deadline, deadline).await
        {
            first_error.get_or_insert(error);
        }
    }
    pending_cleanup.retain_sessions(sessions, reservation);
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(())
    }
}

/// 先请求 SSH disconnect 并 join；已被 owner 取消的 socket 只重试 join，不向失效队列重发。
/// 只有截止时间前仍未 join 的 Handle 才继续持有 transport；连接结束后的错误只作诊断。
///
/// @author kongweiguang
async fn disconnect_one_session_until(
    session: &Arc<RusshPeerSession>,
    reason: &str,
    graceful_deadline: tokio::time::Instant,
    hard_deadline: tokio::time::Instant,
) -> AppResult<()> {
    if session.disconnected.load(Ordering::Acquire) {
        return Ok(());
    }

    let mut handle = match tokio::time::timeout_at(graceful_deadline, session.handle.lock()).await {
        Ok(handle) => handle,
        Err(_) => {
            session.transport_cancel.cancel();
            tokio::time::timeout_at(hard_deadline, session.handle.lock())
                .await
                .map_err(|_| AppError::timeout("SSH session handle lock"))?
        }
    };
    // 另一个 owner 可能在等待 mutex 时已 join task，因此再次检查后不重 poll 已完成 oneshot。
    if session.disconnected.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut first_error = None;
    if !session.transport_cancel.is_cancelled()
        && !session.disconnect_requested.load(Ordering::Acquire)
    {
        match tokio::time::timeout_at(
            graceful_deadline,
            handle.disconnect(russh::Disconnect::ByApplication, reason, ""),
        )
        .await
        {
            Ok(Ok(())) => session.disconnect_requested.store(true, Ordering::Release),
            Ok(Err(error)) => {
                first_error = Some(russh_error("SSH session disconnect request failed", error));
            }
            Err(_) => {
                first_error = Some(AppError::timeout("SSH session disconnect request"));
            }
        }
    }

    let graceful_exit = tokio::time::timeout_at(
        graceful_deadline,
        std::future::poll_fn(|context| Pin::new(&mut *handle).poll(context)),
    )
    .await;
    let exit = match graceful_exit {
        Ok(exit) => Some(exit),
        Err(_) => {
            session.transport_cancel.cancel();
            tokio::time::timeout_at(
                hard_deadline,
                std::future::poll_fn(|context| Pin::new(&mut *handle).poll(context)),
            )
            .await
            .ok()
        }
    };

    if exit.is_some() {
        // 只在 Handle Future 真正完成后置位，避免重试时重复 poll 已完成的 oneshot receiver。
        session.disconnected.store(true, Ordering::Release);
    }
    session_cleanup_result(
        exit,
        session.disconnect_requested.load(Ordering::Acquire),
        session.transport_cancel.is_cancelled(),
        first_error,
    )
}

/// 只有未在截止时间内 join 的会话才仍持有 socket 与 russh task。
///
/// Handle Future 完成即证明会话任务与底层 transport 已经结束，此时无论 join 返回
/// 什么错误都不存在可继续清理的资源；若仍把它报告为失败，插件重启或更新安装会
/// 因为“已关闭资源”被假失败挡住并让用户手动重试。已 join 的异常只保留脱敏诊断。
///
/// @author kongweiguang
fn session_cleanup_result(
    exit: Option<Result<(), russh::Error>>,
    disconnect_requested: bool,
    transport_cancelled: bool,
    first_error: Option<AppError>,
) -> AppResult<()> {
    let Some(exit) = exit else {
        return Err(first_error.unwrap_or_else(|| AppError::timeout("SSH session task shutdown")));
    };
    if let Some(error) = first_error {
        tracing::debug!(
            code = error.public_code().as_str(),
            "SSH disconnect request failed before the session task finished"
        );
    }
    if let Err(error) = exit
        && !is_expected_local_disconnect(disconnect_requested, transport_cancelled, &error)
        && !is_confirmed_remote_transport_end(&error)
    {
        let diagnostic = russh_error("SSH session task failed", error);
        tracing::warn!(
            code = diagnostic.public_code().as_str(),
            "SSH session task ended with an error after join; transport resources are released"
        );
    }
    Ok(())
}

/// 只分类 Handle join 后的终止结果；调用方不得用它豁免仍运行的 task、channel writer 或非终止 IO 错误。
///
/// @author kongweiguang
fn is_confirmed_remote_transport_end(error: &russh::Error) -> bool {
    match error {
        russh::Error::Disconnect => true,
        russh::Error::IO(error) => {
            !error
                .get_ref()
                .is_some_and(|source| source.is::<LocalSshTransportCancelled>())
                && matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::UnexpectedEof
                )
        }
        _ => false,
    }
}

/// 只接受本地已入队 disconnect sentinel 或已取消 socket 的私有 IO source，保留其它已知任务错误。
///
/// @author kongweiguang
fn is_expected_local_disconnect(
    disconnect_requested: bool,
    transport_cancelled: bool,
    error: &russh::Error,
) -> bool {
    match error {
        russh::Error::Disconnect => disconnect_requested,
        russh::Error::IO(error) => {
            transport_cancelled
                && error.kind() == io::ErrorKind::ConnectionAborted
                && error
                    .get_ref()
                    .is_some_and(|source| source.is::<LocalSshTransportCancelled>())
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ssh_disconnect.rs"]
mod disconnect_tests;

#[cfg(test)]
#[path = "../../tests/unit/ssh_agent_error.rs"]
mod agent_error_tests;

// 单元测试需要 RSA 夹具；夹具只有常量，避免把集成测试的 app 适配层编进 lib。
#[cfg(test)]
#[path = "../../tests/support/ssh_test_keys.rs"]
#[allow(dead_code, reason = "单元测试只需 RSA，其余密钥由集成测试使用")]
mod ssh_test_keys;

#[cfg(test)]
#[path = "../../tests/unit/ssh_rsa_negotiation.rs"]
mod rsa_negotiation_tests;

#[cfg(test)]
#[path = "../../tests/unit/ssh_proxy_reference.rs"]
mod proxy_reference_tests;

#[cfg(test)]
#[path = "../../tests/unit/ssh_remote_forward.rs"]
mod remote_forward_tests;

/// 尽力关闭单个失败通道并设置固定上界，清理路径不能吞掉外层请求的错误。
async fn close_channel_best_effort(channel: &mut russh::Channel<Msg>) {
    let _ = tokio::time::timeout(CLOSE_OPERATION_TIMEOUT, channel.close()).await;
}

/// 在 request session channel 上等待服务器 Success/Failure，避免把未受理的 exec 当作成功启动。
async fn await_session_request_reply(
    channel: russh::Channel<Msg>,
    cancel: &CancellationToken,
    timeout: Duration,
) -> AppResult<(russh::Channel<Msg>, Vec<CommandEvent>)> {
    let mut channel = channel;
    let mut initial_events = Vec::new();
    let mut initial_bytes = 0usize;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            close_channel_best_effort(&mut channel).await;
            return Err(AppError::timeout("SSH exec request confirmation"));
        }
        let message = tokio::select! {
            _ = cancel.cancelled() => { close_channel_best_effort(&mut channel).await; return Err(AppError::cancelled("SSH session request cancelled")); }
            result = tokio::time::timeout(remaining, channel.wait()) => match result {
                Ok(message) => message,
                Err(_) => { close_channel_best_effort(&mut channel).await; return Err(AppError::timeout("SSH exec request confirmation")); }
            },
        };
        match message {
            Some(ChannelMsg::Success) => return Ok((channel, initial_events)),
            Some(ChannelMsg::Failure) => {
                close_channel_best_effort(&mut channel).await;
                return Err(AppError::unsupported(
                    "Remote server rejected the SSH exec request",
                ));
            }
            Some(ChannelMsg::Data { data }) => {
                initial_bytes = initial_bytes.saturating_add(data.len());
                if initial_bytes > 1024 * 1024 {
                    close_channel_best_effort(&mut channel).await;
                    return Err(AppError::infrastructure(
                        "SSH exec setup output exceeded its buffer limit",
                    ));
                }
                initial_events.push(CommandEvent::Stdout(data.to_vec()));
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                initial_bytes = initial_bytes.saturating_add(data.len());
                if initial_bytes > 1024 * 1024 {
                    close_channel_best_effort(&mut channel).await;
                    return Err(AppError::infrastructure(
                        "SSH exec setup output exceeded its buffer limit",
                    ));
                }
                initial_events.push(CommandEvent::Stderr(data.to_vec()));
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                initial_events.push(CommandEvent::Exited {
                    code: Some(exit_status),
                    signal: None,
                })
            }
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                initial_events.push(CommandEvent::Exited {
                    code: None,
                    signal: Some(format!("{signal_name:?}")),
                })
            }
            Some(ChannelMsg::Eof) => {}
            Some(ChannelMsg::Close) | None => {
                return Err(AppError::infrastructure(
                    "SSH exec channel closed before request confirmation",
                ));
            }
            Some(_) => {}
        }
    }
}

/// 等待 SFTP subsystem 受理，并保留确认前已到达的协议字节以免丢失初始化包。
async fn await_subsystem_reply(
    mut channel: russh::Channel<Msg>,
    cancel: &CancellationToken,
    timeout: Duration,
) -> AppResult<(russh::Channel<Msg>, Vec<u8>)> {
    let mut initial_data = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            close_channel_best_effort(&mut channel).await;
            return Err(AppError::timeout("SSH SFTP subsystem confirmation"));
        }
        let message = tokio::select! {
            _ = cancel.cancelled() => { close_channel_best_effort(&mut channel).await; return Err(AppError::cancelled("SSH SFTP subsystem request cancelled")); }
            result = tokio::time::timeout(remaining, channel.wait()) => match result {
                Ok(message) => message,
                Err(_) => { close_channel_best_effort(&mut channel).await; return Err(AppError::timeout("SSH SFTP subsystem confirmation")); }
            },
        };
        match message {
            Some(ChannelMsg::Success) => return Ok((channel, initial_data)),
            Some(ChannelMsg::Failure) => {
                close_channel_best_effort(&mut channel).await;
                return Err(AppError::capability_not_supported("SFTP subsystem"));
            }
            Some(ChannelMsg::Data { data }) => {
                if initial_data.len().saturating_add(data.len()) > MAX_SUBSYSTEM_SETUP_BYTES {
                    close_channel_best_effort(&mut channel).await;
                    return Err(AppError::infrastructure(
                        "SSH SFTP setup output exceeded its buffer limit",
                    ));
                }
                initial_data.extend_from_slice(&data)
            }
            Some(ChannelMsg::ExtendedData { .. }) => {}
            Some(ChannelMsg::Close) | None => {
                return Err(AppError::infrastructure(
                    "SSH SFTP channel closed before subsystem confirmation",
                ));
            }
            Some(_) => {}
        }
    }
}

/// 在 profile 缺省 timeout 时使用 30 秒；显式的零值不允许形成无限连接等待。
fn connect_timeout(timeout_ms: u64) -> Duration {
    if timeout_ms == 0 {
        DEFAULT_CONNECT_TIMEOUT
    } else {
        Duration::from_millis(timeout_ms)
    }
}

/// 连接能力必须在读取秘密和打开 socket 前完成门禁。
fn require_capability(
    capabilities: ConnectionCapabilities,
    intent: ConnectionIntent,
) -> AppResult<()> {
    let (supported, name) = match intent {
        ConnectionIntent::Terminal => (capabilities.terminal, "terminal"),
        ConnectionIntent::Exec => (capabilities.exec, "exec"),
        ConnectionIntent::Sftp => (capabilities.sftp, "sftp"),
        ConnectionIntent::Tunnel => (capabilities.tunnel, "tunnel"),
    };
    if supported {
        Ok(())
    } else {
        Err(AppError::capability_not_supported(name))
    }
}

/// 验证 host profile 结构，不把未配置的 key、endpoint 或方言自动猜成可连接目标。
fn validate_profile(profile: &HostConnectionProfile) -> AppResult<()> {
    validate_endpoint(&profile.endpoint)?;
    validate_proxy_credential_reference(profile)?;
    if profile.connect_timeout_ms > 10 * 60 * 1000 {
        return Err(AppError::invalid(
            "SSH connection timeout exceeds the supported limit",
        ));
    }
    peer_specs(profile)?;
    Ok(())
}

/// 只允许代理密码引用绑定当前 HostId、proxy-host kind 与密码材料，阻断跨槽复用。
fn validate_proxy_credential_reference(profile: &HostConnectionProfile) -> AppResult<()> {
    let Some(proxy) = profile.route.proxy.as_ref() else {
        return Ok(());
    };
    let Some(reference) = proxy.password.as_ref() else {
        return Ok(());
    };
    let parsed = CredentialReference::parse(reference.clone())
        .map_err(|_| AppError::invalid("代理凭据引用无效"))?;
    if parsed.kind().as_str() != "proxy-host"
        || parsed.host_id() != &profile.host_id
        || parsed.material() != CredentialMaterial::Password
        || !is_proxy_transaction_scope(parsed.scope().as_str())
        || proxy.endpoint.username.trim().is_empty()
    {
        return Err(AppError::invalid("代理凭据引用与当前连接不匹配"));
    }
    Ok(())
}

/// 接受应用层唯一 scope 的 `proxy-` 加 32 位小写 UUID 形状，拒绝任意扩展 scope。
fn is_proxy_transaction_scope(scope: &str) -> bool {
    let Some(suffix) = scope.strip_prefix("proxy-") else {
        return false;
    };
    suffix.len() == 32
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 限制该 adapter 到 SSH wire protocol；Telnet、Serial 和 RDP 由各自协议适配器处理。
fn require_ssh_protocol(profile: &HostConnectionProfile) -> AppResult<()> {
    if matches!(profile.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
        Ok(())
    } else {
        Err(AppError::capability_not_supported("SSH transport"))
    }
}

/// 校验连接地址和登录名的控制字符，避免协议字段被意外拆分。
fn validate_endpoint(endpoint: &HostEndpoint) -> AppResult<()> {
    endpoint
        .validate()
        .map_err(|_| AppError::invalid("SSH endpoint is invalid"))
}

/// 校验 direct-tcpip 地址，不把它当 shell 文本或任意本地命令处理。
fn validate_tunnel_endpoint(
    endpoint: &kerminal_domain::connections::TunnelEndpoint,
) -> AppResult<()> {
    if endpoint.host.trim().is_empty()
        || endpoint.port == 0
        || endpoint.host.chars().any(char::is_control)
    {
        return Err(AppError::invalid("SSH tunnel destination is invalid"));
    }
    Ok(())
}

/// 检查 remote-forward 的 bind address 字段，以失败关闭未知协议值。
fn validate_tunnel_target(endpoint: &HostEndpoint) -> AppResult<()> {
    validate_endpoint(endpoint)
}

/// 将 shell 编码失败转成固定安全文案，不把用户参数写入错误上下文。
fn exec_encoding_error(error: super::exec::ExecEncodingError) -> AppError {
    match error {
        super::exec::ExecEncodingError::UnknownShellDialect => {
            AppError::unsupported("Remote shell dialect is unknown; structured exec is disabled")
        }
        _ => AppError::invalid("Structured SSH exec request is invalid"),
    }
}

/// 保存 peer owner 的 stream adapter，使 channel drop 只结束该通道而不丢失其 jump chain。
struct OwnedPeerStream {
    inner: Box<dyn AsyncDuplex>,
    _peer: Arc<RusshPeer>,
}
impl OwnedPeerStream {
    /// 创建 stream owner 以延长 SSH lease 的父会话寿命。
    fn new(inner: Box<dyn AsyncDuplex>, peer: Arc<RusshPeer>) -> Self {
        Self { inner, _peer: peer }
    }
}
impl AsyncRead for OwnedPeerStream {
    /// 将 channel 字节原样读取，背压由 Russh channel window 自然传递。
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for OwnedPeerStream {
    /// 向 direct-tcpip channel 写入数据，不合并或重排调用方的 buffer。
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    /// 刷新当前 channel writer。
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    /// 半关闭该 TCP 转发方向，不关闭共享 SSH parent connection。
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 重放 subsystem 确认前的数据，再直接透传后续 channel stream，避免 SFTP 初始化包丢失。
struct PrefixedPeerStream {
    prefix: std::collections::VecDeque<u8>,
    inner: Box<dyn AsyncDuplex>,
    _peer: Arc<RusshPeer>,
}
impl PrefixedPeerStream {
    /// 初始前缀仅包含 SSH channel 中已经读取的有限协议数据；后续字节不做二次缓存。
    fn new(inner: Box<dyn AsyncDuplex>, initial_data: Vec<u8>, peer: Arc<RusshPeer>) -> Self {
        Self {
            prefix: initial_data.into(),
            inner,
            _peer: peer,
        }
    }
}
impl AsyncRead for PrefixedPeerStream {
    /// 先交付已读前缀，队列耗尽后直接从 channel stream 读取。
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() > 0 && !this.prefix.is_empty() {
            let count = {
                let prefix = this.prefix.make_contiguous();
                let count = buf.remaining().min(prefix.len());
                buf.put_slice(&prefix[..count]);
                count
            };
            this.prefix.drain(..count);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for PrefixedPeerStream {
    /// SFTP 上传直接写入当前 channel，避免把整文件复制进中间缓冲。
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    /// 等待当前 SFTP channel writer 中已排队的数据发出。
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    /// 结束当前 SFTP 通道方向，不影响连接 lease 的其它通道。
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 每个 forwarded-tcpip channel 由转发用例逐条接收，并独立管理其流生命周期。
struct RusshRemoteForwardListener {
    session: Arc<RusshPeerSession>,
    router: Arc<ForwardRouter>,
    receiver: mpsc::Receiver<russh::Channel<Msg>>,
    bind_host: String,
    bind_port: u16,
    closed: bool,
    peer: Arc<RusshPeer>,
    operation_timeout: Duration,
}

#[async_trait]
impl RemoteForwardListener for RusshRemoteForwardListener {
    /// 等待下一条已授权端口的 channel 或显式取消，不轮询网络 socket。
    async fn accept(
        &mut self,
        cancel: CancellationToken,
    ) -> AppResult<Option<TunnelChannelHandle>> {
        let channel = tokio::select! {
            _ = cancel.cancelled() => return Ok(None),
            channel = self.receiver.recv() => channel,
        };
        Ok(channel.map(|channel| {
            TunnelChannelHandle::new(Box::new(OwnedPeerStream::new(
                Box::new(channel.into_stream()),
                Arc::clone(&self.peer),
            )))
        }))
    }

    /// cancel 入队前隔离该端口，仅成功 ACK 释放隔离；失败保留本 listener owner 供显式重试。
    ///
    /// @author kongweiguang
    async fn close(&mut self) -> AppResult<()> {
        if self.closed {
            return Ok(());
        }
        self.router.retire(u32::from(self.bind_port))?;
        let result = tokio::time::timeout(self.operation_timeout, async {
            self.session
                .handle
                .lock()
                .await
                .cancel_tcpip_forward(self.bind_host.clone(), u32::from(self.bind_port))
                .await
        })
        .await;
        match result {
            Ok(Ok(())) => {
                self.router.confirm_cancel(self.bind_port);
                self.closed = true;
                Ok(())
            }
            Ok(Err(error)) => Err(russh_error("SSH remote-forward cancel failed", error)),
            Err(_) => Err(AppError::timeout("SSH remote-forward cancel")),
        }
    }

    /// 返回 server 选择的实际端口，动态请求的零端口已经被替换为分配值。
    fn bind_endpoint(&self) -> (&str, u16) {
        (&self.bind_host, self.bind_port)
    }
}

/// 错误只保留固定上下文和 source；不把底层协议文本拼入用户可见详情。
pub(super) fn russh_error(
    context: &'static str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> AppError {
    AppError::infrastructure_with_source(context, error)
}
