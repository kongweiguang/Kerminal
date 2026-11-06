//! SFTP INIT 取消必须释放锁定版 client 的后台 reader，并准确报告 lease 清理结果。
//!
//! @author kongweiguang

use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::{
        ChannelLane, CommandStreamHandle, ExecRequest, OpenSftpRequest, RemoteConnectionHandle,
        RemoteConnectionLease, RemoteForwardHandle, RemoteForwardRequest, SftpChannelHandle,
        ShellScriptRequest, TerminalOpenRequest, TunnelChannelHandle, TunnelRequest,
    },
    sessions::SessionTransport,
};
use kerminal_domain::{
    connections::{ConnectionCapabilities, RemoteProtocol, RemoteShellDialect},
    identity::HostId,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::oneshot,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::{
    RawSftpInitializationError, SftpClientInitializationError, initialize_raw_sftp_session,
    initialize_sftp_client, initialize_sftp_session_on_lease,
};

struct ProbeStream {
    inner: DuplexStream,
    request_written: Option<oneshot::Sender<()>>,
    reply_waiting: Option<oneshot::Sender<()>>,
    dropped: Option<oneshot::Sender<()>>,
}

impl AsyncRead for ProbeStream {
    /// 记录真实 SFTP reader 已开始等待服务端回包，再把读取转发给挂起通道。
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some(sender) = self.reply_waiting.take() {
            let _ = sender.send(());
        }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for ProbeStream {
    /// 记录 INIT 数据确实经过 client writer 后转发，避免测试只取消尚未开始的任务。
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if matches!(&result, Poll::Ready(Ok(written)) if *written > 0)
            && let Some(sender) = self.request_written.take()
        {
            let _ = sender.send(());
        }
        result
    }

    /// 将 flush 转发给用于测试的 duplex channel。
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    /// 将 half-close 转发，模拟 SFTP 子系统在 INIT 取消后的 channel 关闭。
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Drop for ProbeStream {
    /// 后台 reader 真正释放 duplex stream 时发回收回执，而不是只检查 UI cancellation token。
    fn drop(&mut self) {
        if let Some(sender) = self.dropped.take() {
            let _ = sender.send(());
        }
    }
}

struct HangingSftpLease {
    host_id: HostId,
    channel: Mutex<Option<SftpChannelHandle>>,
    server: Mutex<Option<DuplexStream>>,
    close_calls: AtomicUsize,
    close_fails: bool,
}

#[async_trait]
impl RemoteConnectionHandle for HangingSftpLease {
    /// 返回 fixture 固定的连接身份。
    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// 该 fixture 只允许 SFTP，避免测试意外借用 shell 或终端能力。
    fn capabilities(&self) -> ConnectionCapabilities {
        ConnectionCapabilities::for_protocol(RemoteProtocol::Sftp)
    }

    /// SFTP-only fixture 不提供 shell 方言。
    fn shell_dialect(&self) -> RemoteShellDialect {
        RemoteShellDialect::Unknown
    }

    /// 初始化取消测试不打开 terminal。
    async fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        Err(AppError::unsupported("test terminal unavailable"))
    }

    /// 初始化取消测试不打开 exec stream。
    async fn exec_stream(
        &self,
        _request: ExecRequest,
        _cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        Err(AppError::unsupported("test exec unavailable"))
    }

    /// 初始化取消测试不执行 shell script。
    async fn exec_shell(
        &self,
        _request: ShellScriptRequest,
        _cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        Err(AppError::unsupported("test shell unavailable"))
    }

    /// 提供一次真实 duplex SFTP channel，服务端不回 VERSION 以挂起实际 INIT 握手。
    async fn open_sftp(
        &self,
        _request: OpenSftpRequest,
        cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("test SFTP open cancelled"));
        }
        self.channel
            .lock()
            .map_err(|_| AppError::infrastructure("test channel lock poisoned"))?
            .take()
            .ok_or_else(|| AppError::conflict("test SFTP channel already consumed"))
    }

    /// 初始化取消测试不打开 tunnel。
    async fn open_tunnel(
        &self,
        _request: TunnelRequest,
        _cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        Err(AppError::unsupported("test tunnel unavailable"))
    }

    /// 初始化取消测试不创建远端 listener。
    async fn request_remote_forward(
        &self,
        _request: RemoteForwardRequest,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        Err(AppError::unsupported("test forwarding unavailable"))
    }

    /// 关闭 lease 会结束仍未响应的 server half；可模拟 transport owner 未确认 close。
    async fn close_until(&self, _deadline: tokio::time::Instant, _reason: &str) -> AppResult<()> {
        self.close_calls.fetch_add(1, Ordering::AcqRel);
        drop(
            self.server
                .lock()
                .map_err(|_| AppError::infrastructure("test server lock poisoned"))?
                .take(),
        );
        if self.close_fails {
            Err(AppError::infrastructure("test close receipt unavailable"))
        } else {
            Ok(())
        }
    }
}

/// 构造真实 SFTP client 可写 INIT、但必须等待外部 VERSION 的 channel 和两个生命周期回执。
///
/// @author kongweiguang
fn hanging_channel() -> (
    SftpChannelHandle,
    DuplexStream,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
) {
    let (client, server) = tokio::io::duplex(4096);
    let (request_written, request_written_rx) = oneshot::channel();
    let (reply_waiting, reply_waiting_rx) = oneshot::channel();
    let (dropped, dropped_rx) = oneshot::channel();
    let stream = ProbeStream {
        inner: client,
        request_written: Some(request_written),
        reply_waiting: Some(reply_waiting),
        dropped: Some(dropped),
    };
    (
        SftpChannelHandle::new(Box::new(stream)),
        server,
        request_written_rx,
        reply_waiting_rx,
        dropped_rx,
    )
}

/// 等待 INIT 已写出且 reader 已进入 VERSION 等待，固定取消测试的真实协议时序。
///
/// @author kongweiguang
async fn wait_for_hanging_init(
    request_written: oneshot::Receiver<()>,
    reply_waiting: oneshot::Receiver<()>,
) {
    timeout(Duration::from_secs(2), request_written)
        .await
        .expect("SFTP INIT request should be written")
        .expect("SFTP INIT writer should remain alive");
    timeout(Duration::from_secs(2), reply_waiting)
        .await
        .expect("SFTP INIT should wait for VERSION")
        .expect("SFTP INIT reader should remain alive");
}

/// 读取一个完整 SFTP frame，测试 server 用它确认锁定版 client 的 INIT/EXTENDED 请求顺序。
///
/// @author kongweiguang
async fn read_sftp_packet(server: &mut DuplexStream) -> std::io::Result<Vec<u8>> {
    let mut length = [0; 4];
    server.read_exact(&mut length).await?;
    let mut packet = vec![0; u32::from_be_bytes(length) as usize];
    server.read_exact(&mut packet).await?;
    Ok(packet)
}

/// 发出声明 limits 扩展的 VERSION，使真实 client 在 INIT 后继续等待 limits reply。
///
/// @author kongweiguang
async fn send_version_with_limits(server: &mut DuplexStream) -> std::io::Result<()> {
    let name = b"limits@openssh.com";
    let version = b"1";
    let payload_len = 1 + 4 + 4 + name.len() + 4 + version.len();
    let mut packet = Vec::with_capacity(4 + payload_len);
    packet.extend_from_slice(&(payload_len as u32).to_be_bytes());
    packet.push(2);
    packet.extend_from_slice(&3_u32.to_be_bytes());
    packet.extend_from_slice(&(name.len() as u32).to_be_bytes());
    packet.extend_from_slice(name);
    packet.extend_from_slice(&(version.len() as u32).to_be_bytes());
    packet.extend_from_slice(version);
    server.write_all(&packet).await
}

/// 挂起真实初始化并取消，断言旧 reader 收到 lease 关闭且清理错误不会伪装成取消成功。
///
/// @author kongweiguang
async fn cancel_provider_initialization(close_fails: bool) -> (AppError, usize) {
    let host_id = HostId::new();
    let (channel, server, request_written, reply_waiting, dropped) = hanging_channel();
    let handle = Arc::new(HangingSftpLease {
        host_id,
        channel: Mutex::new(Some(channel)),
        server: Mutex::new(Some(server)),
        close_calls: AtomicUsize::new(0),
        close_fails,
    });
    let lease = RemoteConnectionLease::new(handle.clone());
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(initialize_sftp_session_on_lease(
        lease,
        ChannelLane::Bulk,
        cancellation.clone(),
        russh_sftp::client::Config::default(),
    ));

    wait_for_hanging_init(request_written, reply_waiting).await;
    cancellation.cancel();
    let result = timeout(Duration::from_secs(2), task)
        .await
        .expect("canceled SFTP initialization should finish")
        .expect("SFTP initialization worker should remain joinable");
    let error = match result {
        Ok(_) => panic!("canceled SFTP initialization must not return a session"),
        Err(error) => error,
    };
    timeout(Duration::from_secs(2), dropped)
        .await
        .expect("lease close should release the SFTP reader")
        .expect("SFTP reader drop should be observed");

    (error, handle.close_calls.load(Ordering::Acquire))
}

/// INIT 等待 VERSION 时取消必须关闭本次 lease，并等到后台 reader 真实释放。
///
/// @author kongweiguang
#[tokio::test]
async fn canceled_sftp_init_closes_lease_and_releases_reader() {
    let (error, close_calls) = cancel_provider_initialization(false).await;

    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert_eq!(close_calls, 1);
}

/// lease close 无法确认时保留基础设施资源故障，不能返回看似成功的取消错误。
///
/// @author kongweiguang
#[tokio::test]
async fn canceled_sftp_init_reports_unconfirmed_lease_close() {
    let (error, close_calls) = cancel_provider_initialization(true).await;

    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(close_calls, 1);
}

/// raw INIT 被取消时发出本次 channel 的 close，并等待读写 task 都因 half-close 退出。
///
/// @author kongweiguang
#[tokio::test]
async fn canceled_raw_init_closes_only_its_channel() {
    let (channel, mut server, request_written, reply_waiting, dropped) = hanging_channel();
    let server_task = tokio::spawn(async move {
        let mut bytes = [0; 128];
        while server
            .read(&mut bytes)
            .await
            .expect("server read should succeed")
            > 0
        {}
    });
    let session = russh_sftp::client::RawSftpSession::new(channel);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let cancellation = cancellation.clone();
        async move { initialize_raw_sftp_session(&session, &cancellation).await }
    });

    wait_for_hanging_init(request_written, reply_waiting).await;
    cancellation.cancel();
    assert!(matches!(
        timeout(Duration::from_secs(2), task)
            .await
            .expect("raw SFTP INIT cancellation should finish")
            .expect("raw SFTP INIT worker should remain joinable"),
        Err(RawSftpInitializationError::Cancelled)
    ));
    timeout(Duration::from_secs(2), server_task)
        .await
        .expect("raw channel close should end the server half")
        .expect("test server should close after client half-close");
    timeout(Duration::from_secs(2), dropped)
        .await
        .expect("raw channel close should release the client reader")
        .expect("raw reader drop should be observed");
}

/// VERSION 后 limits INIT 请求已发出但无回包时取消，必须仍释放 client reader 和 channel。
///
/// @author kongweiguang
#[tokio::test]
async fn canceled_limits_negotiation_releases_client_reader() {
    let (channel, mut server, _request_written, _reply_waiting, dropped) = hanging_channel();
    let (limits_requested, limits_requested_rx) = oneshot::channel();
    let server_task = tokio::spawn(async move {
        let init = read_sftp_packet(&mut server)
            .await
            .expect("server should receive INIT");
        assert_eq!(init.first(), Some(&1));
        send_version_with_limits(&mut server)
            .await
            .expect("server should send VERSION");
        let limits = read_sftp_packet(&mut server)
            .await
            .expect("server should receive limits request");
        assert_eq!(limits.first(), Some(&200));
        let _ = limits_requested.send(());
        let mut bytes = [0; 128];
        while server
            .read(&mut bytes)
            .await
            .expect("server read should succeed")
            > 0
        {}
    });
    let cancellation = CancellationToken::new();
    let worker = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            initialize_sftp_client(
                channel,
                &cancellation,
                russh_sftp::client::Config::default(),
            )
            .await
        }
    });

    timeout(Duration::from_secs(2), limits_requested_rx)
        .await
        .expect("limits request should be sent after VERSION")
        .expect("limits request sender should remain alive");
    cancellation.cancel();
    assert!(matches!(
        timeout(Duration::from_secs(2), worker)
            .await
            .expect("limits cancellation should return")
            .expect("limits worker should remain joinable"),
        Err(SftpClientInitializationError::Cancelled)
    ));
    timeout(Duration::from_secs(2), server_task)
        .await
        .expect("client channel close should end the server half")
        .expect("test server should close after client half-close");
    timeout(Duration::from_secs(2), dropped)
        .await
        .expect("limits cancellation should release the reader")
        .expect("client reader drop should be observed");
}
