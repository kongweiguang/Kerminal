//! Tokio TCP listener、SSH direct-tcpip 与 SOCKS5 的四类转发数据面。
//!
//! @author kongweiguang

use std::{net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use fast_socks5::{
    ReplyError, Socks5Command, server::Socks5ServerProtocol, util::target_addr::TargetAddr,
};
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::{
        ConnectionIntent, RemoteConnectionLease, RemoteConnectionService, RemoteForwardHandle,
        RemoteForwardRequest, TunnelChannelHandle, TunnelRequest,
    },
    operations::{PortForwardRuntime, PortForwardRuntimeHandle},
};
use kerminal_domain::{
    connections::TunnelEndpoint,
    operations::{ForwardEndpoint, ForwardKind, PortForwardConfig},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
    time::timeout_at,
};
use tokio_util::sync::CancellationToken;

const SOCKS5_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const SOCKS5_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 为 Local、Remote、Dynamic、RemoteDynamic 复用同一 SSH lease 与 channel 能力。
pub struct SshPortForwardRuntime {
    connections: Arc<dyn RemoteConnectionService>,
}

impl SshPortForwardRuntime {
    /// 端口数据面只依赖连接应用端口，不自行解析凭据或创建第二条 SSH transport。
    pub fn new(connections: Arc<dyn RemoteConnectionService>) -> Self {
        Self { connections }
    }
}

#[async_trait]
impl PortForwardRuntime for SshPortForwardRuntime {
    /// 按转发方向建立本地 listener 或 SSH 远端 listener，并返回独立资源 handle。
    async fn start(
        &self,
        _id: String,
        config: PortForwardConfig,
        cancel: CancellationToken,
    ) -> AppResult<Arc<dyn PortForwardRuntimeHandle>> {
        let lease = self
            .connections
            .connect(
                config.host_id.clone(),
                ConnectionIntent::Tunnel,
                cancel.clone(),
            )
            .await?;
        let runtime_cancel = CancellationToken::new();
        let (shutdown_deadline, shutdown_deadline_rx) = tokio::sync::watch::channel(None);
        let setup = async {
            match config.kind {
                ForwardKind::Local | ForwardKind::Dynamic => {
                    let listener =
                        TcpListener::bind((config.bind.host.as_str(), config.bind.port.get()))
                            .await
                            .map_err(|error| {
                                AppError::infrastructure_with_source("本机端口转发监听失败", error)
                            })?;
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled("端口转发启动已取消"));
                    }
                    let local_addr = listener.local_addr().map_err(|error| {
                        AppError::infrastructure_with_source("读取本机转发端点失败", error)
                    })?;
                    let actual_bind = endpoint_from_socket(local_addr)?;
                    let (task, failure) = spawn_local_forward(
                        listener,
                        lease.clone(),
                        config.clone(),
                        runtime_cancel.clone(),
                        shutdown_deadline_rx.clone(),
                    );
                    Ok((actual_bind, task, failure))
                }
                ForwardKind::Remote | ForwardKind::RemoteDynamic => {
                    let mut listener = lease
                        .request_remote_forward(
                            RemoteForwardRequest {
                                bind_host: config.bind.host.clone(),
                                bind_port: config.bind.port.get(),
                            },
                            cancel.clone(),
                        )
                        .await?;
                    if cancel.is_cancelled() {
                        return match listener.close().await {
                            Ok(()) => Err(AppError::cancelled("端口转发启动已取消")),
                            Err(close_error) => {
                                Err(AppError::cancelled("端口转发启动已取消")
                                    .with_source(close_error))
                            }
                        };
                    }
                    let (host, port) = listener.bind_endpoint();
                    let actual_bind = match ForwardEndpoint::new(host.to_owned(), port) {
                        Ok(endpoint) => endpoint,
                        Err(_) => {
                            let error =
                                AppError::infrastructure("SSH server 返回的转发监听端点无效");
                            return match listener.close().await {
                                Ok(()) => Err(error),
                                Err(close_error) => Err(error.with_source(close_error)),
                            };
                        }
                    };
                    let (task, failure) = spawn_remote_forward(
                        listener,
                        lease.clone(),
                        config.clone(),
                        runtime_cancel.clone(),
                        shutdown_deadline_rx.clone(),
                    );
                    Ok((actual_bind, task, failure))
                }
            }
        }
        .await;
        let (actual_bind, task, failure) = match setup {
            Ok(resources) => resources,
            Err(error) => {
                return match lease
                    .close_until(
                        tokio::time::Instant::now() + Duration::from_secs(5),
                        "端口转发启动失败",
                    )
                    .await
                {
                    Ok(()) => Err(error),
                    Err(close_error) => Err(error.with_source(close_error)),
                };
            }
        };
        Ok(Arc::new(TokioPortForwardHandle::new(
            actual_bind,
            runtime_cancel,
            shutdown_deadline,
            task,
            failure,
        )))
    }
}

struct TokioPortForwardHandle {
    actual_bind: ForwardEndpoint,
    cancel: CancellationToken,
    shutdown_deadline: tokio::sync::watch::Sender<Option<tokio::time::Instant>>,
    task: tokio::sync::Mutex<Option<JoinHandle<AppResult<()>>>>,
    failure: tokio::sync::watch::Receiver<Option<PublicErrorCode>>,
}

impl TokioPortForwardHandle {
    /// 资源句柄拥有单一后台任务；重复 close 共享同一 join 结果且不会重建监听口。
    fn new(
        actual_bind: ForwardEndpoint,
        cancel: CancellationToken,
        shutdown_deadline: tokio::sync::watch::Sender<Option<tokio::time::Instant>>,
        task: JoinHandle<AppResult<()>>,
        failure: tokio::sync::watch::Receiver<Option<PublicErrorCode>>,
    ) -> Self {
        Self {
            actual_bind,
            cancel,
            shutdown_deadline,
            task: tokio::sync::Mutex::new(Some(task)),
            failure,
        }
    }
}

#[async_trait]
impl PortForwardRuntimeHandle for TokioPortForwardHandle {
    /// 返回本机或远端确认的实际 bind endpoint，而非直接回传请求配置。
    fn actual_bind(&self) -> ForwardEndpoint {
        self.actual_bind.clone()
    }

    /// 监听器主循环由 handle 在创建时绑定独立取消 token，失败码由监控任务注入。
    fn subscribe_failure(&self) -> tokio::sync::watch::Receiver<Option<PublicErrorCode>> {
        self.failure.clone()
    }

    /// 以统一绝对 deadline 取消主循环并等待 listener、活动 channel 和 SSH lease 收尾。
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.shutdown_deadline.send_replace(Some(deadline));
        self.cancel.cancel();
        let Some(mut task) = self.task.lock().await.take() else {
            return Ok(());
        };
        match timeout_at(deadline, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(AppError::infrastructure_with_source(
                "端口转发任务异常结束",
                error,
            )),
            Err(error) => {
                *self.task.lock().await = Some(task);
                Err(AppError::infrastructure_with_source(
                    "端口转发资源释放超时",
                    error,
                ))
            }
        }
    }
}

/// 以单独 watcher channel 将底层任务退出原因映射为脱敏稳定错误码。
fn monitored_task<F>(
    cancel: CancellationToken,
    failure: tokio::sync::watch::Sender<Option<PublicErrorCode>>,
    operation: F,
) -> JoinHandle<AppResult<()>>
where
    F: std::future::Future<Output = AppResult<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = operation.await;
        if result.is_err() && !cancel.is_cancelled() {
            let _ = failure.send(Some(PublicErrorCode::Infrastructure));
        }
        result
    })
}

/// 本机 listener 处理固定目标或 SOCKS5 请求，并在关闭时回收全部已接受连接。
fn spawn_local_forward(
    listener: TcpListener,
    lease: RemoteConnectionLease,
    config: PortForwardConfig,
    parent_cancel: CancellationToken,
    shutdown_deadline: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>,
) -> (
    JoinHandle<AppResult<()>>,
    tokio::sync::watch::Receiver<Option<PublicErrorCode>>,
) {
    let cancel = parent_cancel;
    let (failure_tx, failure_rx) = tokio::sync::watch::channel(None);
    let operation_cancel = cancel.clone();
    let task = monitored_task(cancel, failure_tx, async move {
        let destination = config.destination;
        let dynamic = config.kind == ForwardKind::Dynamic;
        run_local_listener(
            listener,
            lease,
            destination,
            dynamic,
            operation_cancel,
            shutdown_deadline,
        )
        .await
    });
    (task, failure_rx)
}

/// SSH 远端 listener 接收新 forwarded-tcpip channel，目的端由配置或 SOCKS5 请求确定。
fn spawn_remote_forward(
    listener: RemoteForwardHandle,
    lease: RemoteConnectionLease,
    config: PortForwardConfig,
    parent_cancel: CancellationToken,
    shutdown_deadline: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>,
) -> (
    JoinHandle<AppResult<()>>,
    tokio::sync::watch::Receiver<Option<PublicErrorCode>>,
) {
    let cancel = parent_cancel;
    let (failure_tx, failure_rx) = tokio::sync::watch::channel(None);
    let operation_cancel = cancel.clone();
    let task = monitored_task(cancel, failure_tx, async move {
        let destination = config.destination;
        let dynamic = config.kind == ForwardKind::RemoteDynamic;
        run_remote_listener(
            listener,
            lease,
            destination,
            dynamic,
            operation_cancel,
            shutdown_deadline,
        )
        .await
    });
    (task, failure_rx)
}

/// 运行本机接入循环，单个目标连接失败只影响当次客户端不终止其它转发。
async fn run_local_listener(
    listener: TcpListener,
    lease: RemoteConnectionLease,
    destination: Option<ForwardEndpoint>,
    dynamic: bool,
    cancel: CancellationToken,
    shutdown_deadline: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>,
) -> AppResult<()> {
    let mut clients = JoinSet::new();
    let clients_cancel = cancel.child_token();
    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            Some(_) = clients.join_next(), if !clients.is_empty() => {},
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => {
                let lease = lease.clone();
                let destination = destination.clone();
                let client_cancel = clients_cancel.child_token();
                clients.spawn(async move {
                    let result = match (dynamic, destination) {
                        (true, _) => proxy_socks(socket, lease, client_cancel).await,
                        (false, Some(destination)) => {
                            proxy_fixed_tcp(socket, lease, destination, client_cancel).await
                        }
                        (false, None) => Err(AppError::invalid("固定转发缺少目标端点")),
                    };
                    if let Err(error) = result {
                        tracing::debug!(error_code = error.public_code().as_str(), "单个本机端口转发连接结束");
                    }
                });
                }
                Err(error) => break Err(AppError::infrastructure_with_source("本机转发监听器失败", error)),
            }
        }
    };
    if result.is_err() {
        clients_cancel.cancel();
    }
    let deadline = current_shutdown_deadline(shutdown_deadline);
    let clients_result = join_clients_until(&mut clients, deadline).await;
    let lease_result = lease.close_until(deadline, "本机端口转发已停止").await;
    combine_cleanup(combine_cleanup(result, clients_result), lease_result)
}

/// 接收 SSH server 转发连接并将其桥接到固定本机目标或 SOCKS5 动态目标。
///
/// listener 的 `None` 同时表示父取消 token 结束 accept，或 SSH server 意外关闭 listener；
/// 两者必须根据父 token 分流，否则正常 stop 会被异步故障监控误标为 Failed。
async fn run_remote_listener(
    mut listener: RemoteForwardHandle,
    lease: RemoteConnectionLease,
    destination: Option<ForwardEndpoint>,
    dynamic: bool,
    cancel: CancellationToken,
    shutdown_deadline: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>,
) -> AppResult<()> {
    let mut clients = JoinSet::new();
    let clients_cancel = cancel.child_token();
    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            Some(_) = clients.join_next(), if !clients.is_empty() => {},
            accepted = listener.accept(cancel.child_token()) => match accepted {
                Ok(Some(channel)) => {
                    let lease = lease.clone();
                    let destination = destination.clone();
                    let client_cancel = clients_cancel.child_token();
                    clients.spawn(async move {
                        let result = match (dynamic, destination) {
                            (true, _) => proxy_socks(channel, lease, client_cancel).await,
                            (false, Some(destination)) => {
                                proxy_fixed_channel(channel, destination, client_cancel).await
                            }
                            (false, None) => Err(AppError::invalid("固定转发缺少目标端点")),
                        };
                        if let Err(error) = result {
                            tracing::debug!(error_code = error.public_code().as_str(), "单个 SSH 转发连接结束");
                        }
                    });
                }
                Ok(None) if cancel.is_cancelled() => break Ok(()),
                Ok(None) => break Err(AppError::infrastructure("SSH 远端转发 listener 意外结束")),
                Err(_) if cancel.is_cancelled() => break Ok(()),
                Err(error) => break Err(error),
            }
        }
    };
    if result.is_err() {
        clients_cancel.cancel();
    }
    let deadline = current_shutdown_deadline(shutdown_deadline);
    let close_result = match timeout_at(deadline, listener.close()).await {
        Ok(result) => result,
        Err(error) => Err(AppError::infrastructure_with_source(
            "SSH 远端转发 listener 撤销超过统一清理时限",
            error,
        )),
    };
    let clients_result = join_clients_until(&mut clients, deadline).await;
    let lease_result = lease.close_until(deadline, "远端端口转发已停止").await;
    combine_cleanup(
        combine_cleanup(combine_cleanup(result, close_result), clients_result),
        lease_result,
    )
}

/// 等待所有已接受的 TCP/SSH channel task 自行处理取消并完成关闭。
async fn join_clients(clients: &mut JoinSet<()>) -> AppResult<()> {
    let mut result = Ok(());
    while let Some(joined) = clients.join_next().await {
        if let Err(error) = joined {
            result = combine_cleanup(
                result,
                Err(AppError::infrastructure_with_source(
                    "端口转发连接任务未能正常退出",
                    error,
                )),
            );
        }
    }
    result
}

/// 让当前 forward 收尾使用 handle 写入的绝对 deadline；异常退出也有固定有界预算。
fn current_shutdown_deadline(
    deadline: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>,
) -> tokio::time::Instant {
    deadline
        .borrow()
        .as_ref()
        .copied()
        .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(5))
}

/// 在统一期限内等待已接受客户端；超时后显式中止并 drain JoinSet，绝不遗弃 task。
async fn join_clients_until(
    clients: &mut JoinSet<()>,
    deadline: tokio::time::Instant,
) -> AppResult<()> {
    match timeout_at(deadline, join_clients(clients)).await {
        Ok(result) => result,
        Err(error) => {
            clients.abort_all();
            let joined = join_clients(clients).await;
            let timeout = Err(AppError::infrastructure_with_source(
                "端口转发活动连接超过统一清理时限",
                error,
            ));
            combine_cleanup(timeout, joined)
        }
    }
}

/// 关闭两侧错误流和独立 SSH lease 时保留前置失败与清理失败两份证据。
fn combine_cleanup<T>(result: AppResult<T>, cleanup: AppResult<()>) -> AppResult<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(error.with_source(cleanup_error)),
    }
}

/// 本机 TCP client 经 SSH direct-tcpip 连接固定目标，传输不在应用层缓存整条流。
async fn proxy_fixed_tcp(
    mut socket: TcpStream,
    lease: RemoteConnectionLease,
    destination: ForwardEndpoint,
    cancel: CancellationToken,
) -> AppResult<()> {
    let mut tunnel = open_tunnel(&lease, &destination, &cancel).await?;
    bridge(&mut socket, &mut tunnel, cancel).await
}

/// SSH forwarded-tcpip channel 连接固定本机目标，退出只释放当前两个流。
async fn proxy_fixed_channel(
    mut channel: TunnelChannelHandle,
    destination: ForwardEndpoint,
    cancel: CancellationToken,
) -> AppResult<()> {
    let mut socket = TcpStream::connect((destination.host.as_str(), destination.port.get()))
        .await
        .map_err(|error| AppError::infrastructure_with_source("转发目标连接失败", error))?;
    bridge(&mut channel, &mut socket, cancel).await
}

/// 用成熟协议实现完成无认证 CONNECT 协商；域名留给 SSH 远端解析以匹配动态转发语义。
///
/// @author kongweiguang
async fn proxy_socks<S>(
    incoming: S,
    lease: RemoteConnectionLease,
    cancel: CancellationToken,
) -> AppResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let negotiation = tokio::time::timeout(SOCKS5_HANDSHAKE_TIMEOUT, async {
        let protocol = Socks5ServerProtocol::accept_no_auth(incoming)
            .await
            .map_err(|error| AppError::infrastructure_with_source("SOCKS5 握手失败", error))?;
        protocol
            .read_command()
            .await
            .map_err(|error| AppError::infrastructure_with_source("SOCKS5 请求解析失败", error))
    });
    let (protocol, command, target) = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SOCKS5 握手已取消")),
        result = negotiation => match result {
            Ok(Ok(parsed)) => parsed,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(AppError::timeout("SOCKS5 握手")),
        }
    };

    if command != Socks5Command::TCPConnect {
        protocol
            .reply_error(&ReplyError::CommandNotSupported)
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("SOCKS5 拒绝不支持的命令失败", error)
            })?;
        return Err(AppError::unsupported("SOCKS5 仅支持 CONNECT"));
    }

    let destination = match socks_target_endpoint(target) {
        Ok(destination) => destination,
        Err(error) => {
            protocol
                .reply_error(&ReplyError::AddressTypeNotSupported)
                .await
                .map_err(|reply_error| {
                    AppError::infrastructure_with_source("SOCKS5 返回目标错误失败", reply_error)
                })?;
            return Err(error);
        }
    };

    let mut tunnel = tokio::select! {
        _ = cancel.cancelled() => {
            protocol.reply_error(&ReplyError::GeneralFailure).await.map_err(|error| {
                AppError::infrastructure_with_source("SOCKS5 返回取消状态失败", error)
            })?;
            return Err(AppError::cancelled("SOCKS5 目标连接已取消"));
        },
        result = tokio::time::timeout(SOCKS5_CONNECT_TIMEOUT, open_tunnel(&lease, &destination, &cancel)) => {
            match result {
                Ok(Ok(tunnel)) => tunnel,
                Ok(Err(error)) => {
                    protocol.reply_error(&ReplyError::GeneralFailure).await.map_err(|reply_error| {
                        AppError::infrastructure_with_source("SOCKS5 返回连接失败状态失败", reply_error)
                    })?;
                    return Err(error);
                }
                Err(_) => {
                    protocol.reply_error(&ReplyError::ConnectionTimeout).await.map_err(|error| {
                        AppError::infrastructure_with_source("SOCKS5 返回超时状态失败", error)
                    })?;
                    return Err(AppError::timeout("SOCKS5 目标连接"));
                }
            }
        }
    };
    let mut incoming = protocol
        .reply_success(SocketAddr::from(([0, 0, 0, 0], 0)))
        .await
        .map_err(|error| AppError::infrastructure_with_source("SOCKS5 返回成功状态失败", error))?;
    bridge(&mut incoming, &mut tunnel, cancel).await
}

/// 将协议库的目标类型映射为应用端点，并保留域名让远端 SSH 服务负责解析。
///
/// @author kongweiguang
fn socks_target_endpoint(target: TargetAddr) -> AppResult<ForwardEndpoint> {
    let (host, port) = match target {
        TargetAddr::Ip(address) => (address.ip().to_string(), address.port()),
        TargetAddr::Domain(domain, port) => (domain, port),
    };
    ForwardEndpoint::new(host, port).map_err(|_| AppError::invalid("SOCKS5 目标端点无效"))
}

/// 通过配置 host/port 打开单独 SSH direct-tcpip channel。
async fn open_tunnel(
    lease: &RemoteConnectionLease,
    destination: &ForwardEndpoint,
    cancel: &CancellationToken,
) -> AppResult<TunnelChannelHandle> {
    lease
        .open_tunnel(
            TunnelRequest {
                destination: TunnelEndpoint {
                    host: destination.host.clone(),
                    port: destination.port.get(),
                },
            },
            cancel.clone(),
        )
        .await
}

/// 用有界的 Tokio 双向拷贝桥接两个流；取消或一侧 EOF 时关闭当前数据通道。
async fn bridge<A, B>(left: &mut A, right: &mut B, cancel: CancellationToken) -> AppResult<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let copy_result = tokio::select! {
        result = copy_bidirectional(left, right) => result
            .map(|_| ())
            .map_err(io_failure),
        _ = cancel.cancelled() => Ok(()),
    };
    let (left_close, right_close) = tokio::join!(left.shutdown(), right.shutdown());
    let left_close = left_close.map_err(io_failure);
    let right_close = right_close.map_err(io_failure);
    combine_cleanup(combine_cleanup(copy_result, left_close), right_close)
}

/// 并发接受本地连接，并在退出时 abort/await 所有转发子任务，防止连接泄漏。
fn endpoint_from_socket(address: SocketAddr) -> AppResult<ForwardEndpoint> {
    ForwardEndpoint::new(address.ip().to_string(), address.port())
        .map_err(|_| AppError::infrastructure("操作系统返回的本机绑定端点无效"))
}

/// 将 socket IO 错误保留为内部 source，但 IPC/MCP 只收到固定安全文案。
fn io_failure(error: std::io::Error) -> AppError {
    AppError::infrastructure_with_source("端口转发数据通道失败", error)
}
