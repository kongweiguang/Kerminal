//! 使用真实锁定版 SFTP 协议验证暂存 marker 的断流取消与确认边界。
//!
//! @author kongweiguang

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use kerminal_application::PublicErrorCode;
use kerminal_domain::identity::HostId;
use russh_sftp::{
    client::SftpSession,
    protocol::{
        Data, ExtendedReply, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode,
        Version,
    },
    server::{Handler, StatusReply},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::Notify,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::{
    RemotePath, read_remote_directory_for_cleanup, read_remote_staging_marker,
    write_remote_staging_marker,
};

#[derive(Default)]
struct MarkerState {
    received: Mutex<Vec<u8>>,
    written: Notify,
    release: CancellationToken,
    closed: Notify,
}

struct MarkerServer {
    state: Arc<MarkerState>,
    stall_write: bool,
    fail_write: bool,
}

impl Handler for MarkerServer {
    type Error = StatusReply;

    /// 未覆盖的协议操作显式拒绝；不声明 fsync，专门覆盖 WRITE ACK 没有请求超时的路径。
    ///
    /// @author kongweiguang
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported.into()
    }

    /// 独占创建是 marker 所有权契约的一部分，测试不得通过覆盖文件放行。
    ///
    /// @author kongweiguang
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _attributes: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        assert_eq!(filename, "/marker");
        assert_eq!(
            flags.bits(),
            (OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE).bits()
        );
        Ok(Handle {
            id,
            handle: "marker".into(),
        })
    }

    /// 先确认客户端确实发送了数据，再模拟连接静默或带敏感文本的协议失败。
    ///
    /// @author kongweiguang
    async fn write(
        &mut self,
        id: u32,
        _handle: String,
        offset: u64,
        bytes: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        assert_eq!(offset, 0);
        self.state
            .received
            .lock()
            .expect("marker bytes")
            .extend(bytes);
        self.state.written.notify_one();
        if self.stall_write {
            self.state.release.cancelled().await;
        }
        if self.fail_write {
            return Ok(Status {
                id,
                status_code: StatusCode::PermissionDenied,
                error_message: "sensitive remote path".into(),
                language_tag: String::new(),
            });
        }
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }

    /// 成功必须收到 CLOSE；仅写入缓冲区或发送 WRITE 不能冒充 marker 已完成。
    ///
    /// @author kongweiguang
    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
        self.state.closed.notify_one();
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }
}

/// 只连接内存 duplex，不访问用户主机、配置根或凭据。
///
/// @author kongweiguang
async fn marker_session(stall_write: bool, fail_write: bool) -> (SftpSession, Arc<MarkerState>) {
    let state = Arc::new(MarkerState::default());
    let (client, server) = tokio::io::duplex(4096);
    russh_sftp::server::run(
        server,
        MarkerServer {
            state: state.clone(),
            stall_write,
            fail_write,
        },
    )
    .await;
    let session = SftpSession::new(client).await.expect("SFTP handshake");
    (session, state)
}

/// 无 fsync 的服务器静默吞掉 WRITE ACK 时，取消仍须及时返回，不能依赖不存在的请求超时。
///
/// @author kongweiguang
#[tokio::test]
async fn stalled_marker_write_ack_observes_cancellation() {
    let (session, state) = marker_session(true, false).await;
    let cancellation = CancellationToken::new();
    let worker_cancel = cancellation.clone();
    let worker = tokio::spawn(async move {
        let result = write_remote_staging_marker(
            &session,
            &RemotePath::parse("/marker").expect("path"),
            b"owner",
            &worker_cancel,
        )
        .await;
        session.close().await.expect("close cancelled SFTP channel");
        result
    });
    timeout(Duration::from_secs(2), state.written.notified())
        .await
        .expect("WRITE arrived");
    cancellation.cancel();
    let result = timeout(Duration::from_secs(1), worker).await;
    state.release.cancel();
    let error = result
        .expect("marker cancellation must not hang")
        .expect("marker task")
        .expect_err("cancelled marker cannot succeed");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert_eq!(*state.received.lock().expect("marker bytes"), b"owner");
}

/// 正常创建须确认全部内容和 CLOSE，取消门禁不改变成功时的协议保证。
///
/// @author kongweiguang
#[tokio::test]
async fn marker_success_confirms_bytes_and_close() {
    let (session, state) = marker_session(false, false).await;
    timeout(
        Duration::from_secs(2),
        write_remote_staging_marker(
            &session,
            &RemotePath::parse("/marker").expect("path"),
            b"owner",
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("marker completed")
    .expect("marker success");
    timeout(Duration::from_secs(1), state.closed.notified())
        .await
        .expect("CLOSE confirmed");
    assert_eq!(*state.received.lock().expect("marker bytes"), b"owner");
    session
        .close()
        .await
        .expect("close successful SFTP channel");
}

/// 协议 WRITE 错误只能公开远端流失败，服务器返回的路径文本留在内部错误链。
///
/// @author kongweiguang
#[tokio::test]
async fn marker_write_failure_identifies_remote_stream_and_redacts_server_text() {
    let (session, _) = marker_session(false, true).await;
    let error = timeout(
        Duration::from_secs(2),
        write_remote_staging_marker(
            &session,
            &RemotePath::parse("/marker").expect("path"),
            b"owner",
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("marker write returned")
    .expect_err("server rejected marker WRITE");
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(error.public_message(), "远端 SFTP 数据流失败");
    assert_eq!(
        std::error::Error::source(&error)
            .expect("underlying I/O error")
            .to_string(),
        "sensitive remote path"
    );
    session.close().await.expect("close failed SFTP channel");
}

struct ReadMarkerServer {
    bytes: Vec<u8>,
    fail_read: bool,
    fail_close: bool,
    closed: Arc<AtomicUsize>,
}

impl Handler for ReadMarkerServer {
    type Error = StatusReply;

    /// 未实现的协议操作拒绝，确保配额测试只使用声明的 SFTP 请求。
    ///
    /// @author kongweiguang
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported.into()
    }

    /// 协商真实客户端的单句柄限制，使未归还的配额在第二次读取时直接暴露。
    ///
    /// @author kongweiguang
    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        version
            .extensions
            .insert("limits@openssh.com".into(), "1".into());
        Ok(version)
    }

    /// 四个无符号 64 位字段对应 OpenSSH limits，最后一个限制同时打开的句柄数。
    ///
    /// @author kongweiguang
    async fn extended(
        &mut self,
        id: u32,
        request: String,
        _data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        assert_eq!(request, "limits@openssh.com");
        let data = [0_u64, 0, 0, 1]
            .into_iter()
            .flat_map(u64::to_be_bytes)
            .collect();
        Ok(Packet::ExtendedReply(ExtendedReply { id, data }))
    }

    /// 单独模拟只读 marker，避免把写入测试的独占创建语义引入此场景。
    ///
    /// @author kongweiguang
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _attributes: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        assert_eq!(filename, "/marker");
        assert_eq!(flags.bits(), OpenFlags::READ.bits());
        Ok(Handle {
            id,
            handle: "read-marker".into(),
        })
    }

    /// 模拟正常内容、协议读取失败和 EOF，覆盖读取本身与关闭请求的次序。
    ///
    /// @author kongweiguang
    async fn read(
        &mut self,
        id: u32,
        _handle: String,
        offset: u64,
        length: u32,
    ) -> Result<Data, Self::Error> {
        if self.fail_read {
            return Err(StatusCode::PermissionDenied.into());
        }
        let start = usize::try_from(offset).expect("bounded test offset");
        if start >= self.bytes.len() {
            return Err(StatusCode::Eof.into());
        }
        let end = (start + length as usize).min(self.bytes.len());
        Ok(Data {
            id,
            data: self.bytes[start..end].to_vec(),
        })
    }

    /// 记录每次关闭请求，也可注入关闭失败以核对首个读取错误不会被覆盖。
    ///
    /// @author kongweiguang
    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        if self.fail_close {
            return Err(StatusCode::Failure.into());
        }
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }
}

/// 使用内存协议连接和真实配额协商，不依赖用户 SSH 配置或外部服务器。
///
/// @author kongweiguang
async fn read_marker_session(
    bytes: Vec<u8>,
    fail_read: bool,
    fail_close: bool,
) -> (SftpSession, Arc<AtomicUsize>) {
    let closed = Arc::new(AtomicUsize::new(0));
    let (client, server) = tokio::io::duplex(4096);
    russh_sftp::server::run(
        server,
        ReadMarkerServer {
            bytes,
            fail_read,
            fail_close,
            closed: Arc::clone(&closed),
        },
    )
    .await;
    (
        SftpSession::new(client).await.expect("SFTP handshake"),
        closed,
    )
}

/// 多轮有界读取必须持续归还客户端单句柄配额，单次 CLOSE 报文不足以证明配额回收。
///
/// @author kongweiguang
#[tokio::test]
async fn marker_reads_reuse_single_client_handle() {
    let (session, closed) = read_marker_session(b"owner".to_vec(), false, false).await;
    let path = RemotePath::parse("/marker").expect("path");
    for _ in 0..8 {
        assert_eq!(
            read_remote_staging_marker(&session, &path)
                .await
                .expect("read"),
            Some(b"owner".to_vec())
        );
    }
    assert_eq!(closed.load(Ordering::SeqCst), 8);
    session.close().await.expect("close SFTP channel");
}

/// 读取失败和超大 marker 都必须确认关闭，才能继续复用同一个受限连接。
///
/// @author kongweiguang
#[tokio::test]
async fn marker_read_failures_and_oversize_release_handle() {
    let path = RemotePath::parse("/marker").expect("path");
    for (bytes, fail_read, expected_code) in [
        (b"owner".to_vec(), true, PublicErrorCode::Infrastructure),
        (vec![b'x'; 1025], false, PublicErrorCode::Conflict),
    ] {
        let (session, closed) = read_marker_session(bytes, fail_read, false).await;
        for _ in 0..4 {
            let error = read_remote_staging_marker(&session, &path)
                .await
                .expect_err("read must fail");
            assert_eq!(error.public_code(), expected_code);
        }
        assert_eq!(closed.load(Ordering::SeqCst), 4);
        session.close().await.expect("close SFTP channel");
    }
}

/// 大小错误与关闭失败并存时保留原大小错误；无法确认的关闭留给 endpoint owner。
///
/// @author kongweiguang
#[tokio::test]
async fn oversized_marker_preserves_error_when_close_fails() {
    let (session, closed) = read_marker_session(vec![b'x'; 1025], false, true).await;
    let error = read_remote_staging_marker(&session, &RemotePath::parse("/marker").expect("path"))
        .await
        .expect_err("oversized marker must fail");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(error.public_message(), "传输暂存 marker 大小无效");
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    session.close().await.expect("close SFTP channel");
}

/// 即使内容读取成功，CLOSE 失败也不能报告 marker 已被安全读取和释放。
///
/// @author kongweiguang
#[tokio::test]
async fn marker_read_requires_close_acknowledgment() {
    let (session, closed) = read_marker_session(b"owner".to_vec(), false, true).await;
    let error = read_remote_staging_marker(&session, &RemotePath::parse("/marker").expect("path"))
        .await
        .expect_err("close failure must be reported");
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    session.close().await.expect("close SFTP channel");
}

#[derive(Default)]
struct CleanupDirectoryState {
    read_started: Notify,
    eof: Notify,
    release_read: CancellationToken,
    closed_handles: AtomicUsize,
}

struct CleanupDirectoryServer {
    state: Arc<CleanupDirectoryState>,
    stall: bool,
    fail: bool,
}

impl Handler for CleanupDirectoryServer {
    type Error = StatusReply;

    /// 测试只实现目录读取所需的协议请求，其他操作必须显式失败。
    ///
    /// @author kongweiguang
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported.into()
    }

    /// 返回固定目录句柄，确保测试覆盖 OPENDIR 成功后的清理边界。
    ///
    /// @author kongweiguang
    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        assert_eq!(path, "/tree");
        Ok(Handle {
            id,
            handle: "directory".into(),
        })
    }

    /// 在 READDIR 已抵达后注入失败或静默，避免测试只覆盖打开前取消。
    ///
    /// @author kongweiguang
    async fn readdir(&mut self, _id: u32, handle: String) -> Result<Name, Self::Error> {
        assert_eq!(handle, "directory");
        self.state.read_started.notify_one();
        if self.stall {
            self.state.release_read.cancelled().await;
        }
        if self.fail || self.stall {
            Err(StatusCode::Failure.into())
        } else {
            Err(StatusCode::Eof.into())
        }
    }

    /// 正常遍历先关闭目录句柄；失败和超时则依赖独立子通道退出回收句柄。
    ///
    /// @author kongweiguang
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        assert_eq!(handle, "directory");
        self.state.closed_handles.fetch_add(1, Ordering::SeqCst);
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }
}

struct EofObservedStream {
    inner: DuplexStream,
    state: Arc<CleanupDirectoryState>,
}

impl AsyncRead for EofObservedStream {
    /// 服务端确认读到 EOF，才能证明客户端确实关闭了本次子通道的写方向。
    ///
    /// @author kongweiguang
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if matches!(&result, Poll::Ready(Ok(()))) && buffer.filled().len() == before {
            self.state.eof.notify_one();
        }
        result
    }
}

impl AsyncWrite for EofObservedStream {
    /// 透传服务端响应，避免探针改变 SFTP 协议行为。
    ///
    /// @author kongweiguang
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    /// 保留 duplex 的正常 flush 语义，确保响应在等待 EOF 前已经送达。
    ///
    /// @author kongweiguang
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    /// 只关闭 fixture 的服务端写方向，客户端子通道寿命仍由被测 helper 控制。
    ///
    /// @author kongweiguang
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 每次构造独立内存子通道，验证时不依赖或关闭真实 SSH lease。
///
/// @author kongweiguang
async fn cleanup_directory_session(
    stall: bool,
    fail: bool,
) -> (SftpSession, Arc<CleanupDirectoryState>) {
    let state = Arc::new(CleanupDirectoryState::default());
    let (client, server) = tokio::io::duplex(4096);
    russh_sftp::server::run(
        EofObservedStream {
            inner: server,
            state: Arc::clone(&state),
        },
        CleanupDirectoryServer {
            state: Arc::clone(&state),
            stall,
            fail,
        },
    )
    .await;
    let session = SftpSession::new(client).await.expect("SFTP handshake");
    (session, state)
}

/// 成功与 READDIR 失败都必须结束独立通道；正常遍历另需确认目录句柄 CLOSE。
///
/// @author kongweiguang
#[tokio::test]
async fn cleanup_directory_success_and_failure_close_their_channels() {
    for fail in [false, true] {
        let (session, state) = cleanup_directory_session(false, fail).await;
        let result = read_remote_directory_for_cleanup(
            session,
            &HostId::new(),
            &RemotePath::parse("/tree").expect("path"),
        )
        .await;
        assert_eq!(result.is_err(), fail);
        assert_eq!(
            state.closed_handles.load(Ordering::SeqCst),
            if fail { 0 } else { 1 }
        );
        timeout(Duration::from_secs(2), state.eof.notified())
            .await
            .expect("server received channel EOF");
    }
}

/// READDIR 已挂起后超时必须释放独占会话，服务端最终观察到该通道 EOF。
///
/// @author kongweiguang
#[tokio::test]
async fn cleanup_directory_timeout_closes_its_channel() {
    let (session, state) = cleanup_directory_session(true, false).await;
    let worker = tokio::spawn(async move {
        timeout(
            Duration::from_secs(1),
            read_remote_directory_for_cleanup(
                session,
                &HostId::new(),
                &RemotePath::parse("/tree").expect("path"),
            ),
        )
        .await
    });
    timeout(Duration::from_secs(1), state.read_started.notified())
        .await
        .expect("READDIR arrived");
    let result = timeout(Duration::from_secs(2), worker)
        .await
        .expect("directory timeout returned")
        .expect("directory task");
    assert!(result.is_err(), "stalled READDIR must time out");
    state.release_read.cancel();
    timeout(Duration::from_secs(2), state.eof.notified())
        .await
        .expect("server received channel EOF after timeout");
    assert_eq!(state.closed_handles.load(Ordering::SeqCst), 0);
}
