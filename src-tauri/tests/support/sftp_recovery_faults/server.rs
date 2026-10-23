//! 支持 ACK 丢失、延迟和断流的 loopback SSH/SFTP 服务端。
//!
//! `bssh-russh-sftp` 的 server dispatcher 按请求串行处理，因此这里将
//! `OutOfOrderAck` 明确实现为按 offset 变化的确认延迟，而不会把串行协议错误地宣称为
//! 真正的乱序响应；乱序 offset tracker 由生产层单测负责。其余模式均运行真实 SSH 握手、
//! SFTP subsystem 和临时文件读写。
//!
//! @author kongweiguang

use russh::{
    keys::{self, PrivateKey},
    server::{Auth, Msg, Server as _, Session},
    Channel, ChannelId,
};
use russh_sftp::protocol::{
    Attrs, Data, File as ProtocolFile, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpListener,
    sync::watch,
    time::sleep,
};

/// 故障服务器在每个真实 SFTP 请求上采用的行为。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum FaultMode {
    /// 所有请求正常完成。
    Normal = 0,
    /// 第一次写请求写入数据但永远不返回确认。
    NeverCompletesWrite = 1,
    /// 第一次读请求永远不返回确认。
    NeverCompletesRead = 2,
    /// 每个读写确认都延迟固定时长。
    DelayedAck = 3,
    /// 第一次写请求写入后返回连接丢失状态。
    Disconnect = 4,
    /// 对不同 offset 使用不同延迟；真实 dispatcher 串行，所以不伪造乱序响应。
    OutOfOrderAck = 5,
    /// 只阻塞一次写请求，后续新连接恢复正常。
    StallOnceWrite = 6,
    /// 每个新连接的第一次写请求都阻塞，用于验证自动恢复第二次失败。
    StallEveryConnection = 7,
    /// 触发 overwrite fallback 后拒绝 partial 到正式目标的提交 rename。
    RejectPartialCommit = 8,
    /// 同时拒绝 fallback 提交与 backup 回滚，制造可恢复的未知状态。
    RejectCommitAndRollback = 9,
    /// 在既有目标已备份后挂起 partial 提交，供取消宽限期验证未知结果保护。
    StallCommitAfterBackup = 10,
}

impl FaultMode {
    /// 测试线程以原子字节切换模式，未知值安全退回正常处理。
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::NeverCompletesWrite,
            2 => Self::NeverCompletesRead,
            3 => Self::DelayedAck,
            4 => Self::Disconnect,
            5 => Self::OutOfOrderAck,
            6 => Self::StallOnceWrite,
            7 => Self::StallEveryConnection,
            8 => Self::RejectPartialCommit,
            9 => Self::RejectCommitAndRollback,
            10 => Self::StallCommitAfterBackup,
            _ => Self::Normal,
        }
    }
}

/// 隔离的 SSH/SFTP 服务端句柄；Drop 会通知所有停滞中的连接退出。
#[derive(Debug)]
pub(crate) struct FaultSftpServer {
    pub(crate) addr: SocketAddr,
    mode: Arc<AtomicU8>,
    write_requests: Arc<AtomicUsize>,
    read_requests: Arc<AtomicUsize>,
    commit_stalls: Arc<AtomicUsize>,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl FaultSftpServer {
    /// 切换后续请求的故障模式，已阻塞的旧请求仍由取消/连接关闭负责收敛。
    pub(crate) fn set_mode(&self, mode: FaultMode) {
        self.mode.store(mode as u8, Ordering::SeqCst);
    }

    /// 返回服务端已经收到的写请求数量，用于避免测试在连接尚未进入阻塞点时发起取消。
    pub(crate) fn write_requests(&self) -> usize {
        self.write_requests.load(Ordering::SeqCst)
    }

    /// 返回服务端已经收到的读请求数量，用于下载取消的确定性同步。
    pub(crate) fn read_requests(&self) -> usize {
        self.read_requests.load(Ordering::SeqCst)
    }

    /// 返回已进入悬停提交 rename 的请求数，避免取消测试只停在 backup 与 partial rename 的间隙。
    pub(crate) fn commit_stalls(&self) -> usize {
        self.commit_stalls.load(Ordering::SeqCst)
    }
}

impl Drop for FaultSftpServer {
    /// 每个停滞请求都监听 shutdown；只停止监听器会遗留独立的 SSH connection handler。
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

#[derive(Clone)]
struct FaultSshServer {
    root: PathBuf,
    mode: Arc<AtomicU8>,
    write_requests: Arc<AtomicUsize>,
    read_requests: Arc<AtomicUsize>,
    commit_stalls: Arc<AtomicUsize>,
    stall_once_consumed: Arc<AtomicU8>,
    shutdown: watch::Receiver<bool>,
}

struct FaultSshSession {
    root: PathBuf,
    mode: Arc<AtomicU8>,
    write_requests: Arc<AtomicUsize>,
    read_requests: Arc<AtomicUsize>,
    commit_stalls: Arc<AtomicUsize>,
    stall_once_consumed: Arc<AtomicU8>,
    shutdown: watch::Receiver<bool>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Server for FaultSshServer {
    type Handler = FaultSshSession;

    /// 每个连接克隆独立 shutdown receiver，fixture Drop 能唤醒其停滞的请求。
    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        FaultSshSession {
            root: self.root.clone(),
            mode: Arc::clone(&self.mode),
            write_requests: Arc::clone(&self.write_requests),
            read_requests: Arc::clone(&self.read_requests),
            commit_stalls: Arc::clone(&self.commit_stalls),
            stall_once_consumed: Arc::clone(&self.stall_once_consumed),
            shutdown: self.shutdown.clone(),
            channels: HashMap::new(),
        }
    }
}

impl russh::server::Handler for FaultSshSession {
    type Error = russh::Error;

    /// 只接受测试用户名和固定临时密码，避免 fixture 依赖用户凭据。
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == "deploy" && password == "secret" {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    /// 保存并确认会话通道，SFTP channel 必须先由 SSH 层显式接受。
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    /// 将已确认的 SSH session 交给真实 SFTP dispatcher；测试故障在文件 handler 注入。
    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let Some(channel) = self.channels.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        russh_sftp::server::run(
            channel.into_stream(),
            FaultSftpFs {
                root: self.root.clone(),
                mode: Arc::clone(&self.mode),
                write_requests: Arc::clone(&self.write_requests),
                read_requests: Arc::clone(&self.read_requests),
                commit_stalls: Arc::clone(&self.commit_stalls),
                stall_once_consumed: Arc::clone(&self.stall_once_consumed),
                shutdown: self.shutdown.clone(),
                next_handle: 0,
                handles: HashMap::new(),
                stalled_connection: false,
                rejected_partial_commit_target: None,
            },
        )
        .await;
        Ok(())
    }
}

struct FaultSftpFs {
    root: PathBuf,
    mode: Arc<AtomicU8>,
    write_requests: Arc<AtomicUsize>,
    read_requests: Arc<AtomicUsize>,
    commit_stalls: Arc<AtomicUsize>,
    stall_once_consumed: Arc<AtomicU8>,
    shutdown: watch::Receiver<bool>,
    next_handle: u64,
    handles: HashMap<String, FaultHandle>,
    stalled_connection: bool,
    rejected_partial_commit_target: Option<String>,
}

enum FaultHandle {
    File { file: fs::File },
}

impl FaultSftpFs {
    /// 约束远程路径只能落在本测试临时根目录内，避免故障 fixture 访问宿主文件。
    fn resolve_path(&self, remote_path: &str) -> Result<PathBuf, StatusCode> {
        let mut local_path = self.root.clone();
        for segment in remote_path.replace('\\', "/").split('/') {
            match segment {
                "" | "." => {}
                ".." => return Err(StatusCode::PermissionDenied),
                segment => local_path.push(segment),
            }
        }
        Ok(local_path)
    }

    /// 生成仅在当前 SFTP channel 内有效的句柄，避免跨连接共享文件状态。
    fn next_handle(&mut self) -> String {
        self.next_handle = self.next_handle.saturating_add(1);
        format!("fault-file-{}", self.next_handle)
    }

    /// 统一转换临时文件系统错误，响应中不包含宿主绝对路径。
    fn io_status(error: io::Error) -> StatusCode {
        match error.kind() {
            io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        }
    }

    /// 构造稳定的成功响应，便于客户端只依赖 SFTP 状态码。
    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_owned(),
            language_tag: "en-US".to_owned(),
        }
    }

    /// 永久等待也要监听 fixture 关闭，避免 russh 独立 connection handler 泄漏到下个测试。
    async fn stall_until_shutdown(&self) -> Result<(), StatusCode> {
        let mut shutdown = self.shutdown.clone();
        if *shutdown.borrow() {
            return Err(StatusCode::ConnectionLost);
        }
        let _ = shutdown.changed().await;
        Err(StatusCode::ConnectionLost)
    }

    /// 按模式等待确认；旧阻塞请求只在 channel 关闭或 fixture 停止时结束。
    async fn maybe_delay_write(&mut self, offset: u64) -> Result<(), StatusCode> {
        let mode = FaultMode::from_raw(self.mode.load(Ordering::SeqCst));
        match mode {
            FaultMode::NeverCompletesWrite => self.stall_until_shutdown().await?,
            FaultMode::StallOnceWrite
                if !self.stalled_connection
                    && self
                        .stall_once_consumed
                        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok() =>
            {
                self.stalled_connection = true;
                self.stall_until_shutdown().await?
            }
            FaultMode::StallEveryConnection if !self.stalled_connection => {
                self.stalled_connection = true;
                self.stall_until_shutdown().await?
            }
            FaultMode::DelayedAck => sleep(Duration::from_millis(65)).await,
            FaultMode::OutOfOrderAck => {
                let delay = if offset == 0 { 250 } else { 10 };
                sleep(Duration::from_millis(delay)).await;
            }
            FaultMode::Disconnect => return Err(StatusCode::ConnectionLost),
            _ => {}
        }
        Ok(())
    }

    /// 按模式等待读确认；首次返回后可将模式切换为 Normal 以模拟恢复。
    async fn maybe_delay_read(&self, offset: u64) -> Result<(), StatusCode> {
        match FaultMode::from_raw(self.mode.load(Ordering::SeqCst)) {
            FaultMode::NeverCompletesRead => self.stall_until_shutdown().await,
            FaultMode::DelayedAck => {
                sleep(Duration::from_millis(65)).await;
                Ok(())
            }
            FaultMode::OutOfOrderAck => {
                let delay = if offset == 0 { 250 } else { 10 };
                sleep(Duration::from_millis(delay)).await;
                Ok(())
            }
            FaultMode::Disconnect => Err(StatusCode::ConnectionLost),
            _ => Ok(()),
        }
    }
}

impl russh_sftp::server::Handler for FaultSftpFs {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    /// 打开远程文件并保留句柄，真实上传/下载路径依赖该生命周期。
    fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> impl std::future::Future<Output = Result<Handle, Self::Error>> + Send {
        let path = self.resolve_path(&filename);
        let handle = self.next_handle();
        let this = self;
        async move {
            let local_path = path?;
            let options: std::fs::OpenOptions = pflags.into();
            let file = options.open(local_path).map_err(Self::io_status)?;
            this.handles.insert(
                handle.clone(),
                FaultHandle::File {
                    file: fs::File::from_std(file),
                },
            );
            Ok(Handle { id, handle })
        }
    }

    /// 关闭文件前 flush，保证测试读取宿主临时根目录时看到服务端已写入字节。
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        let Some(FaultHandle::File { mut file, .. }) = self.handles.remove(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        file.flush().await.map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    /// 读取远端临时文件；模式可使读确认永久挂起以验证下载取消。
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        self.read_requests.fetch_add(1, Ordering::SeqCst);
        self.maybe_delay_read(offset).await?;
        let Some(FaultHandle::File { file, .. }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(Self::io_status)?;
        let mut data = vec![0; len as usize];
        let bytes = file.read(&mut data).await.map_err(Self::io_status)?;
        if bytes == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(bytes);
        Ok(Data { id, data })
    }

    /// 写入前记录请求并按故障模式阻塞/延迟确认；阻塞模式先落盘以留下可验证 partial。
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        self.write_requests.fetch_add(1, Ordering::SeqCst);
        let Some(FaultHandle::File { file, .. }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(Self::io_status)?;
        file.write_all(&data).await.map_err(Self::io_status)?;
        file.flush().await.map_err(Self::io_status)?;
        self.maybe_delay_write(offset).await?;
        Ok(Self::ok(id))
    }

    /// 返回远端文件属性，使可靠写入层能够决定是否续传 partial。
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let local_path = self.resolve_path(&path)?;
        let metadata = fs::metadata(local_path).await.map_err(Self::io_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    /// `SftpSession::metadata` 使用 STAT 查询 partial/final，必须与 LSTAT 返回同一临时属性。
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, path).await
    }

    /// 返回打开句柄的当前大小，供下载和提交阶段做精确尺寸确认。
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some(FaultHandle::File { file, .. }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        let metadata = file.metadata().await.map_err(Self::io_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    /// 可靠写入只需要接受属性更新，不让测试 handler 引入额外权限变量。
    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(Self::ok(id))
    }

    /// 句柄属性更新同样保持幂等，避免干扰传输状态测试。
    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(Self::ok(id))
    }

    /// 清理失败 fixture 的 partial 文件，不影响待验证的正式目标。
    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let local_path = self.resolve_path(&filename)?;
        fs::remove_file(local_path).await.map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    /// Reject overwrite like SFTP v3 servers without POSIX rename and inject deterministic commit
    /// refusals or a missing commit ACK so recovery state is exercised over the real protocol.
    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let old_path = self.resolve_path(&oldpath)?;
        let new_path = self.resolve_path(&newpath)?;
        if fs::symlink_metadata(&new_path).await.is_ok() {
            return Err(StatusCode::Failure);
        }

        let mode = FaultMode::from_raw(self.mode.load(Ordering::SeqCst));
        let is_partial = oldpath.ends_with(".kerminal-part");
        if matches!(
            mode,
            FaultMode::RejectPartialCommit | FaultMode::RejectCommitAndRollback
        ) && is_partial
            && self.rejected_partial_commit_target.is_none()
        {
            self.rejected_partial_commit_target = Some(newpath);
            return Err(StatusCode::Failure);
        }
        if mode == FaultMode::RejectCommitAndRollback
            && self.rejected_partial_commit_target.as_deref() == Some(newpath.as_str())
            && !is_partial
        {
            return Err(StatusCode::Failure);
        }
        if mode == FaultMode::StallCommitAfterBackup && is_partial {
            // The destination was proven absent above; the fixture starts with an old final so
            // reaching this partial rename proves the backup move already completed.
            self.commit_stalls.fetch_add(1, Ordering::SeqCst);
            self.stall_until_shutdown().await?;
        }
        fs::rename(old_path, new_path)
            .await
            .map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    /// 提供客户端可能调用的 realpath，返回原始逻辑路径而不泄露本地根目录。
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![ProtocolFile::dummy(path)],
        })
    }
}

/// 启动监听随机 loopback 端口的故障 SSH/SFTP 服务端。
pub(crate) async fn start_fault_server(root: PathBuf, mode: FaultMode) -> FaultSftpServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind SFTP recovery fault server");
    let addr = listener
        .local_addr()
        .expect("read SFTP recovery fault server address");
    let private_key = PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)
        .expect("generate SFTP recovery test host key");
    let config = russh::server::Config {
        auth_rejection_time: Duration::from_millis(0),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        keys: vec![private_key.clone()],
        maximum_packet_size: 65_535,
        ..Default::default()
    };
    let mode = Arc::new(AtomicU8::new(mode as u8));
    let write_requests = Arc::new(AtomicUsize::new(0));
    let read_requests = Arc::new(AtomicUsize::new(0));
    let commit_stalls = Arc::new(AtomicUsize::new(0));
    let stall_once_consumed = Arc::new(AtomicU8::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = FaultSshServer {
        root,
        mode: Arc::clone(&mode),
        write_requests: Arc::clone(&write_requests),
        read_requests: Arc::clone(&read_requests),
        commit_stalls: Arc::clone(&commit_stalls),
        stall_once_consumed: Arc::clone(&stall_once_consumed),
        shutdown: shutdown_rx,
    };
    let task = tokio::spawn(async move {
        let mut server = server;
        let _ = server.run_on_socket(Arc::new(config), &listener).await;
    });

    FaultSftpServer {
        addr,
        mode,
        write_requests,
        read_requests,
        commit_stalls,
        shutdown: shutdown_tx,
        task,
    }
}
