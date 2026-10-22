//! 可供连接、命令与文件集成测试共用的隔离 russh server 和应用端口适配器。
//!
//! server 私钥是 russh 上游公开测试夹具，仅用于 loopback 测试，不得作为产品凭据复用。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CredentialResolver, HostConnectionRepository, HostKeyIdentity, HostKeyStatus,
        KnownHostsRepository, RemoteShellKind, ResolvedCredential,
    },
};
use kerminal_domain::{
    connections::{HostConnectionProfile, HostEndpoint},
    identity::HostId,
};
use russh::server::{self, Auth, Server as _};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const TEST_SERVER_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQQ/i+HCsmZZPy0JhtT64vW7EmeA1DeA
M/VnPq3vAhu+xooJ7IMMK3lUHlBDosyvA2enNbCWyvNQc25dVt4oh9RhAAAAqHG7WMFxu1
jBAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBD+L4cKyZlk/LQmG
1Pri9bsSZ4DUN4Az9Wc+re8CG77GignsgwwreVQeUEOizK8DZ6c1sJbK81Bzbl1W3iiH1G
EAAAAgLAmXR6IlN0SdiD6o8qr+vUr0mXLbajs/m0UlegElOmoAAAANcm9iZXJ0QGJic2Rl
dgECAw==
-----END OPENSSH PRIVATE KEY-----";

/// 控制测试服务接受认证与 channel request 的行为。
#[derive(Clone)]
pub struct SshServerOptions {
    pub accept_password: bool,
    pub accept_public_key: bool,
    pub allow_direct_tcpip: bool,
    /// 接受服务端 remote-forward global request，供客户端端口回执映射测试使用。
    pub accept_remote_forward: bool,
    /// port=0 请求时服务端返回的分配端口；固定端口按 RFC 不返回端口字段。
    pub remote_forward_assigned_port: u32,
    /// 延迟第一次 remote-forward 回执，稳定覆盖客户端取消与 pending 路由清理。
    pub remote_forward_first_request_delay: Duration,
    pub reject_pty: bool,
    pub reject_shell: bool,
    pub reject_exec: bool,
    pub accept_sftp_subsystem: bool,
    pub sftp_setup_stdout: Vec<u8>,
    pub sftp_setup_stderr: Vec<u8>,
    pub exec_burst_chunks: usize,
    /// PTY 回应延迟，用于在服务端已收到请求后确定性取消客户端 open。
    pub pty_request_delay: Duration,
    /// shell 请求已接受后立即关闭 channel，不发送 exit-status 或 exit-signal。
    pub close_shell_without_exit_status: bool,
    /// shell 请求已接受后发送真实 ExitSignal 并关闭 channel。
    pub send_shell_exit_signal: bool,
    /// 模拟只读 nonce shell probe 的应答类型；Unknown 继续使用普通 exec fixture 响应。
    pub shell_probe_kind: RemoteShellKind,
}

impl Default for SshServerOptions {
    /// 默认允许密码和公钥认证以及基本请求，方便测试仅覆盖单一失败分支。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            accept_password: true,
            accept_public_key: true,
            allow_direct_tcpip: false,
            accept_remote_forward: false,
            remote_forward_assigned_port: 41_321,
            remote_forward_first_request_delay: Duration::ZERO,
            reject_pty: false,
            reject_shell: false,
            reject_exec: false,
            accept_sftp_subsystem: true,
            sftp_setup_stdout: Vec::new(),
            sftp_setup_stderr: Vec::new(),
            exec_burst_chunks: 0,
            pty_request_delay: Duration::ZERO,
            close_shell_without_exit_status: false,
            send_shell_exit_signal: false,
            shell_probe_kind: RemoteShellKind::Unknown,
        }
    }
}

/// 正在 loopback 监听的 russh server；销毁时取消 accept loop，不连接或修改用户 SSH 配置。
pub struct SshServerFixture {
    pub address: SocketAddr,
    pub fingerprint_sha256: String,
    pub exec_commands: Arc<StdMutex<Vec<Vec<u8>>>>,
    pub terminal_input: Arc<StdMutex<Vec<Vec<u8>>>>,
    pub accepted_connections: Arc<AtomicUsize>,
    pub closed_connections: Arc<AtomicUsize>,
    pub pty_requests: Arc<AtomicUsize>,
    pub remote_forward_cancellations: Arc<StdMutex<Vec<(String, u32)>>>,
    pub remote_forward_request_count: Arc<AtomicUsize>,
    connection_abort_handles: Arc<StdMutex<Vec<tokio::task::AbortHandle>>>,
    shutdown: CancellationToken,
    accept_task: Option<JoinHandle<()>>,
}

impl SshServerFixture {
    /// 绑定动态 loopback 端口并启动真实 SSH handshake，避免单测依赖系统 sshd。
    /// accepted task 的 abort handle 由 fixture 持有，使硬断只影响本用例连接并可等待 drop 计数。
    ///
    /// @author kongweiguang
    pub async fn spawn(options: SshServerOptions) -> AppResult<Self> {
        let private_key =
            russh::keys::decode_secret_key(TEST_SERVER_KEY, None).map_err(|error| {
                AppError::infrastructure_with_source("测试 SSH key 无法解析", error)
            })?;
        let fingerprint_sha256 = private_key
            .public_key()
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|error| {
            AppError::infrastructure_with_source("测试 SSH listener 创建失败", error)
        })?;
        let address = listener.local_addr().map_err(|error| {
            AppError::infrastructure_with_source("测试 SSH 地址读取失败", error)
        })?;
        let mut config = server::Config::default();
        config.keys.push(private_key);
        config.inactivity_timeout = None;
        let config = Arc::new(config);
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let exec_commands = Arc::new(StdMutex::new(Vec::new()));
        let terminal_input = Arc::new(StdMutex::new(Vec::new()));
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let closed_connections = Arc::new(AtomicUsize::new(0));
        let pty_requests = Arc::new(AtomicUsize::new(0));
        let remote_forward_cancellations = Arc::new(StdMutex::new(Vec::new()));
        let remote_forward_request_count = Arc::new(AtomicUsize::new(0));
        let connection_abort_handles = Arc::new(StdMutex::new(Vec::new()));
        let server = FixtureServer {
            options: Arc::new(options),
            exec_commands: Arc::clone(&exec_commands),
            terminal_input: Arc::clone(&terminal_input),
            pty_requests: Arc::clone(&pty_requests),
            remote_forward_cancellations: Arc::clone(&remote_forward_cancellations),
            remote_forward_request_count: Arc::clone(&remote_forward_request_count),
        };
        let task_accepted_connections = Arc::clone(&accepted_connections);
        let task_closed_connections = Arc::clone(&closed_connections);
        let task_connection_abort_handles = Arc::clone(&connection_abort_handles);
        let accept_task = tokio::spawn(async move {
            let mut server = server;
            loop {
                tokio::select! {
                    _ = task_shutdown.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            task_accepted_connections.fetch_add(1, Ordering::Relaxed);
                            let config = Arc::clone(&config);
                            let handler = server.new_client(None);
                            let closed_connections = Arc::clone(&task_closed_connections);
                            let connection_task = tokio::spawn(async move {
                                let _closed = FixtureConnectionClosed(closed_connections);
                                let _ = server::run_stream(config, stream, handler).await;
                            });
                            task_connection_abort_handles
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .push(connection_task.abort_handle());
                        }
                        Err(_) => break,
                    }
                }
            }
        });
        Ok(Self {
            address,
            fingerprint_sha256,
            exec_commands,
            terminal_input,
            accepted_connections,
            closed_connections,
            pty_requests,
            remote_forward_cancellations,
            remote_forward_request_count,
            connection_abort_handles,
            shutdown,
            accept_task: Some(accept_task),
        })
    }

    /// 硬断所有已接受 SSH TCP task 并等待 task-drop 计数，覆盖没有 SSH Close 包的整连接 EOF。
    ///
    /// @author kongweiguang
    pub async fn hard_disconnect_accepted_connections(&self) -> AppResult<()> {
        let accepted = self.accepted_connections.load(Ordering::Relaxed);
        if accepted == 0 {
            return Err(AppError::conflict(
                "测试尚未接受 SSH TCP 连接，不能执行硬断验证",
            ));
        }
        let handles = self
            .connection_abort_handles
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        for handle in handles {
            handle.abort();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.closed_connections.load(Ordering::Relaxed) < accepted {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .map_err(|_| AppError::timeout("测试 SSH TCP 断开未完成"))?;
        Ok(())
    }

    /// 明确停止测试 listener 并等待 accept loop 收尾，避免并发用例留下后台端口。
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.accept_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }
}

/// 在正常结束或硬取消时都记录 accepted socket task 的真实释放。
struct FixtureConnectionClosed(Arc<AtomicUsize>);

impl Drop for FixtureConnectionClosed {
    /// 把 task future 被 abort 的 drop 也计为 socket 已关闭，供测试等待真实回收。
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for SshServerFixture {
    /// 测试提前失败时仍通知 loopback listener 停止接收新连接。
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct FixtureServer {
    options: Arc<SshServerOptions>,
    exec_commands: Arc<StdMutex<Vec<Vec<u8>>>>,
    terminal_input: Arc<StdMutex<Vec<Vec<u8>>>>,
    pty_requests: Arc<AtomicUsize>,
    remote_forward_cancellations: Arc<StdMutex<Vec<(String, u32)>>>,
    remote_forward_request_count: Arc<AtomicUsize>,
}

impl server::Server for FixtureServer {
    type Handler = FixtureHandler;

    /// 每个连接使用独立 handler，但共享只读选项和命令记录器。
    ///
    /// @author kongweiguang
    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        FixtureHandler {
            options: Arc::clone(&self.options),
            exec_commands: Arc::clone(&self.exec_commands),
            terminal_input: Arc::clone(&self.terminal_input),
            pty_requests: Arc::clone(&self.pty_requests),

            remote_forward_cancellations: Arc::clone(&self.remote_forward_cancellations),
            remote_forward_request_count: Arc::clone(&self.remote_forward_request_count),
        }
    }
}

struct FixtureHandler {
    options: Arc<SshServerOptions>,
    exec_commands: Arc<StdMutex<Vec<Vec<u8>>>>,
    terminal_input: Arc<StdMutex<Vec<Vec<u8>>>>,
    pty_requests: Arc<AtomicUsize>,
    remote_forward_cancellations: Arc<StdMutex<Vec<(String, u32)>>>,
    remote_forward_request_count: Arc<AtomicUsize>,
}

impl server::Handler for FixtureHandler {
    type Error = russh::Error;

    /// 测试用户固定为 kerminal，密码固定为 test-password，不从环境变量读取真实凭据。
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(
            if self.options.accept_password && user == "kerminal" && password == "test-password" {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }

    /// 测试 fixture 只验证公钥认证协议流，不模拟真实服务器的 authorized_keys 策略。
    async fn auth_publickey(
        &mut self,
        _user: &str,
        _public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if self.options.accept_public_key {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    /// 只回写动态分配端口；计数器供重试同步与动态端口递增，固定端口无需保留重复请求历史。
    ///
    /// @author kongweiguang
    async fn tcpip_forward(
        &mut self,
        _address: &str,
        port: &mut u32,
        _session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        let request_number = self
            .remote_forward_request_count
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if request_number == 1 && !self.options.remote_forward_first_request_delay.is_zero() {
            tokio::time::sleep(self.options.remote_forward_first_request_delay).await;
        }
        if !self.options.accept_remote_forward {
            return Ok(false);
        }
        if *port == 0 {
            *port = self
                .options
                .remote_forward_assigned_port
                .saturating_add(u32::try_from(request_number.saturating_sub(1)).unwrap_or(0));
        }
        Ok(true)
    }

    /// 记录 SSH client 撤销的准确端口，使固定与动态 listener close 都能被验证。
    ///
    /// @author kongweiguang
    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.remote_forward_cancellations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push((address.to_owned(), port));
        Ok(true)
    }

    /// 接受 session channel，后续 PTY、shell、exec 与 subsystem request 单独确认。
    async fn channel_open_session(
        &mut self,
        _channel: russh::Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    /// 仅允许 fixture 经 SSH 跳板连接 loopback 目标，避免测试 server 变成任意 TCP 代理。
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: russh::Channel<server::Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if !self.options.allow_direct_tcpip
            || !matches!(host_to_connect, "127.0.0.1" | "localhost" | "::1")
        {
            drop(reply);
            return Ok(());
        }
        let Ok(port) = u16::try_from(port_to_connect) else {
            drop(reply);
            return Ok(());
        };
        let Ok(mut target) = tokio::net::TcpStream::connect((host_to_connect, port)).await else {
            drop(reply);
            return Ok(());
        };
        reply.accept().await;
        tokio::spawn(async move {
            let mut channel = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut channel, &mut target).await;
        });
        Ok(())
    }

    /// 按用例选择确认或拒绝 PTY，验证客户端没有把 request send 成功当成服务器接受。
    ///
    /// @author kongweiguang
    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.pty_requests.fetch_add(1, Ordering::Relaxed);
        if !self.options.pty_request_delay.is_zero() {
            tokio::time::sleep(self.options.pty_request_delay).await;
        }
        if self.options.reject_pty {
            session.channel_failure(channel)?;
        } else {
            session.channel_success(channel)?;
        }
        Ok(())
    }

    /// 按用例选择确认或拒绝交互 shell request。
    /// 可选模拟远端异常关闭 PTY，确保客户端能区分真实退出与无退出消息的断线。
    ///
    /// @author kongweiguang
    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.options.reject_shell {
            session.channel_failure(channel)?;
        } else {
            session.channel_success(channel)?;
            if self.options.close_shell_without_exit_status {
                session.close(channel)?;
            } else if self.options.send_shell_exit_signal {
                session.exit_signal_request(channel, russh::Sig::TERM, false, "", "")?;
                session.eof(channel)?;
                session.close(channel)?;
            }
        }
        Ok(())
    }

    /// 记录 exec payload；已识别的交互启动命令只确认请求并保持 channel，供客户端生命周期测试使用。
    ///
    /// @author kongweiguang
    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.exec_commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(command.to_vec());
        if self.options.reject_exec {
            session.channel_failure(channel)?;
            return Ok(());
        }
        session.channel_success(channel)?;
        if is_interactive_shell_startup(command) {
            return Ok(());
        }
        if let Some(marker) = shell_probe_marker(command, self.options.shell_probe_kind) {
            let mut response = marker;
            response.push(b'\n');
            session.data(channel, response)?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        }
        if self.options.exec_burst_chunks == 0 {
            session.data(channel, b"fixture-stdout".to_vec())?;
        } else {
            for _ in 0..self.options.exec_burst_chunks {
                session.data(channel, b"x".to_vec())?;
            }
        }
        session.extended_data(channel, 1, b"fixture-stderr".to_vec())?;
        session.exit_status_request(channel, 7)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    /// 记录 PTY 客户端输入，验证 exec 启动 hook 时没有把初始化脚本混入用户键盘字节。
    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        data: &[u8],
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.terminal_input
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(data.to_vec());
        Ok(())
    }

    /// 接受或拒绝 SFTP subsystem 建立请求；fixture 不实现后续 SFTP packet 协议。
    async fn subsystem_request(
        &mut self,
        channel: russh::ChannelId,
        name: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" && self.options.accept_sftp_subsystem {
            if !self.options.sftp_setup_stderr.is_empty() {
                session.extended_data(channel, 1, self.options.sftp_setup_stderr.clone())?;
            }
            if !self.options.sftp_setup_stdout.is_empty() {
                session.data(channel, self.options.sftp_setup_stdout.clone())?;
            }
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }
}

/// 只让四种已知交互启动入口保活；fixture 不执行脚本，普通有限 exec 与 nonce 探测仍正常退出。
///
/// @author kongweiguang
fn is_interactive_shell_startup(command: &[u8]) -> bool {
    let signatures: [&[u8]; 4] = [
        b"bash --posix --login -i",
        b"zsh -l -i",
        b"fish --login --interactive --init-command",
        b"$__kerminal_args = @('-NoLogo', '-NoExit', '-Command', $__kerminal_child_command)",
    ];
    signatures.iter().any(|signature| {
        command
            .windows(signature.len())
            .any(|window| window == *signature)
    })
}

/// 仅从测试命令中的随机 nonce marker 回显配置 shell，不执行探测脚本文本。
fn shell_probe_marker(command: &[u8], kind: RemoteShellKind) -> Option<Vec<u8>> {
    let prefix: &[u8] = match kind {
        RemoteShellKind::Bash => b"KERM_BASH_",
        RemoteShellKind::Zsh => b"KERM_ZSH_",
        RemoteShellKind::Fish => b"KERM_FISH_",
        RemoteShellKind::PowerShell => b"KERM_POWERSHELL_",
        RemoteShellKind::Other => b"KERM_POSIX_",
        RemoteShellKind::Unknown => return None,
    };
    let start = command
        .windows(prefix.len())
        .position(|window| window == prefix)?;
    let suffix = &command[start..];
    let length = suffix
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    Some(suffix[..length].to_vec())
}

/// 让集成测试替换 profile，而不接触具体的配置文件或用户目录。
pub struct StaticHostRepository {
    profile: HostConnectionProfile,
}

impl StaticHostRepository {
    /// 保存单个测试 profile，保证每项调用都按显式 HostId 绑定。
    pub fn new(profile: HostConnectionProfile) -> Self {
        Self { profile }
    }
}

#[async_trait]
impl HostConnectionRepository for StaticHostRepository {
    /// 只返回预先提供的 profile，错误 ID 不会隐式回退到测试目标。
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        if &self.profile.host_id == host_id {
            Ok(self.profile.clone())
        } else {
            Err(AppError::not_found("测试主机不存在"))
        }
    }
}

enum TestSecret {
    Password(String),
    PrivateKey(Vec<u8>),
}

/// 隔离凭据解析器，记录调用次数以验证能力门禁和 host-key 校验先于取密。
#[derive(Default)]
pub struct TestCredentialResolver {
    secrets: StdMutex<HashMap<String, TestSecret>>,
    pub resolve_count: AtomicUsize,
}

impl TestCredentialResolver {
    /// 加入测试密码；秘密仅保存在测试进程内，不输出 Debug 或写入配置文件。
    pub fn insert_password(&self, reference: &str, password: &str) {
        self.secrets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                reference.to_owned(),
                TestSecret::Password(password.to_owned()),
            );
    }

    /// 加入测试私钥字节，用于验证运行态 key auth path。
    pub fn insert_private_key(&self, reference: &str, key: &[u8]) {
        self.secrets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(reference.to_owned(), TestSecret::PrivateKey(key.to_vec()));
    }
}

#[async_trait]
impl CredentialResolver for TestCredentialResolver {
    /// 取消时不返回秘密，且由 Zeroizing 包装所有测试凭据副本。
    async fn resolve(
        &self,
        reference: &kerminal_domain::connections::CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        self.resolve_count.fetch_add(1, Ordering::Relaxed);
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试凭据解析已取消"));
        }
        let secrets = self
            .secrets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match secrets.get(reference.as_str()) {
            Some(TestSecret::Password(value)) => {
                Ok(ResolvedCredential::Password(Zeroizing::new(value.clone())))
            }
            Some(TestSecret::PrivateKey(value)) => Ok(ResolvedCredential::PrivateKey(
                Zeroizing::new(value.clone()),
            )),
            None => Err(AppError::not_found("测试凭据不存在")),
        }
    }
}

/// 按 endpoint 隔离 known_hosts 状态，覆盖 unknown/known/changed 三种路径。
#[derive(Default)]
pub struct TestKnownHostsRepository {
    entries: StdMutex<HashMap<String, (String, String)>>,
}

impl TestKnownHostsRepository {
    /// 在测试开始前注入可信指纹，用于普通连接和变化检测用例。
    pub fn trust_fingerprint(&self, endpoint: &HostEndpoint, algorithm: &str, fingerprint: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                endpoint_key(endpoint),
                (algorithm.to_owned(), fingerprint.to_owned()),
            );
    }
}

#[async_trait]
impl KnownHostsRepository for TestKnownHostsRepository {
    /// unknown/changed 指纹不会自动信任，只有测试显式写入后才返回 Known。
    async fn inspect(
        &self,
        endpoint: &HostEndpoint,
        identity: &HostKeyIdentity,
    ) -> AppResult<HostKeyStatus> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Ok(match entries.get(&endpoint_key(endpoint)) {
            Some((_, current)) if current == &identity.fingerprint_sha256 => HostKeyStatus::Known,
            Some((_, previous)) => HostKeyStatus::Changed {
                previous_fingerprint_sha256: previous.clone(),
            },
            None => HostKeyStatus::Unknown,
        })
    }

    /// 将应用层已二次确认的指纹写入当前测试实例。
    async fn trust(&self, endpoint: &HostEndpoint, identity: &HostKeyIdentity) -> AppResult<()> {
        self.trust_fingerprint(endpoint, &identity.algorithm, &identity.fingerprint_sha256);
        Ok(())
    }

    /// 在隔离的内存 fixture 内做旧指纹 CAS 替换，供 SSH 轮换测试验证回读而不接触用户 known_hosts。
    ///
    /// @author kongweiguang
    async fn replace(
        &self,
        endpoint: &HostEndpoint,
        expected_previous_fingerprint_sha256: &str,
        identity: &HostKeyIdentity,
    ) -> AppResult<()> {
        if expected_previous_fingerprint_sha256.is_empty()
            || expected_previous_fingerprint_sha256 == identity.fingerprint_sha256
        {
            return Err(kerminal_application::AppError::invalid(
                "test host-key rotation fingerprints are invalid",
            ));
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let endpoint = endpoint_key(endpoint);
        match entries.get(&endpoint) {
            Some((_, current)) if current == &identity.fingerprint_sha256 => return Ok(()),
            Some((_, current)) if current == expected_previous_fingerprint_sha256 => {}
            _ => {
                return Err(kerminal_application::AppError::conflict(
                    "test known_hosts key changed before rotation",
                ));
            }
        }
        entries.insert(
            endpoint,
            (
                identity.algorithm.clone(),
                identity.fingerprint_sha256.clone(),
            ),
        );
        Ok(())
    }
}

/// 创建 SSH service 时显式注入 profile、凭据和可信主机仓储。
pub fn connection_service(
    profile: HostConnectionProfile,
    credentials: Arc<dyn CredentialResolver>,
    known_hosts: Arc<dyn KnownHostsRepository>,
) -> Arc<kerminal_infrastructure::connections::RusshRemoteConnectionService> {
    Arc::new(
        kerminal_infrastructure::connections::RusshRemoteConnectionService::new(
            kerminal_infrastructure::connections::RusshRemoteConnectionDependencies {
                hosts: Arc::new(StaticHostRepository::new(profile)),
                credentials,
                known_hosts,
            },
        ),
    )
}

/// 返回仅用于本进程 loopback 测试的上游 fixture 私钥副本。
pub fn test_client_private_key() -> Vec<u8> {
    TEST_SERVER_KEY.as_bytes().to_vec()
}

/// 用稳定 endpoint 元组隔离 known_hosts 项，不依赖系统 HOME 或 ssh config。
fn endpoint_key(endpoint: &HostEndpoint) -> String {
    format!("{}:{}:{}", endpoint.host, endpoint.port, endpoint.username)
}
