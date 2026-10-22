//! 远程连接端口。所有通道均拥有独立生命周期，避免终端、命令和传输互相阻塞。
//!
//! @author kongweiguang

pub mod credentials;
pub mod leased_session_transport;
pub mod management;
pub mod probe;
pub mod rdp_launch_status;

use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use kerminal_domain::{
    connections::{
        ConnectionCapabilities, CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol,
        TunnelEndpoint,
    },
    identity::HostId,
    sessions::TerminalSize,
};

pub use kerminal_domain::connections::{ChannelLane, RemoteShellDialect};
pub use leased_session_transport::LeasedSessionTransportControl;
pub use rdp_launch_status::{
    RdpLaunchId, RdpLaunchStatus, RdpLaunchStatusMessage, RdpLaunchStatusService,
    RdpLaunchStatusSubscription,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{AppError, AppResult, sessions::SessionTransport};

/// 隐去具体 socket 类型，避免应用层绑定某种网络实现。
pub trait AsyncDuplex: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncDuplex for T {}

/// 连接时所需能力；服务必须在读取凭据或建立网络连接前校验目标协议。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionIntent {
    Terminal,
    Exec,
    Sftp,
    Tunnel,
}

/// 当前已认证连接中经过只读 nonce 探测确认的 shell 实现类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteShellKind {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Other,
    Unknown,
}

/// 服务端公钥身份；它是公开验证材料，不是认证密钥，Debug 仍隐藏正文以减少日志噪声。
#[derive(Clone, Eq, PartialEq)]
pub struct HostKeyIdentity {
    pub algorithm: String,
    pub fingerprint_sha256: String,
    /// 完整 OpenSSH 公钥字段，由当前 SSH 握手观察得到，用于写入有效 known_hosts 记录。
    pub public_key_openssh: String,
}

impl std::fmt::Debug for HostKeyIdentity {
    /// 主机公钥虽非秘密，但摘要和公钥正文重复且不适合出现在错误日志里。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostKeyIdentity")
            .field("algorithm", &self.algorithm)
            .field("fingerprint_sha256", &self.fingerprint_sha256)
            .field("public_key_openssh", &"[omitted]")
            .finish()
    }
}

/// 已检查的 SSH 主机指纹挑战，可安全展示给用户；不包含认证材料。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKeyChallenge {
    pub host_id: HostId,
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub algorithm: String,
    pub fingerprint_sha256: String,
    pub identity: HostKeyIdentity,
    pub status: HostKeyStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostKeyPeer {
    Target,
    Jump { index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostKeyStatus {
    Unknown,
    Changed { previous_fingerprint_sha256: String },
    Known,
}

/// 用户确认的指纹绑定；infra 重新握手并逐字段比对后才会写 known_hosts。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKeyTrustRequest {
    pub host_id: HostId,
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub expected_fingerprint_sha256: String,
}

/// 已完成二次校验并持久化的显式信任回执。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKeyTrustReceipt {
    pub host_id: HostId,
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub fingerprint_sha256: String,
}

/// 主机密钥轮换请求把账号、逐跳 peer、网络端点和前后指纹绑定到一次明确操作。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKeyRotationRequest {
    pub host_id: HostId,
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub expected_previous_fingerprint_sha256: String,
    pub expected_fingerprint_sha256: String,
}

/// 轮换回执供 UI 校验目标后再继续原连接意图。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKeyRotationReceipt {
    pub host_id: HostId,
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub fingerprint_sha256: String,
}

/// 凭据解析器只向 SSH adapter 交付短生命周期的运行态凭据。
pub enum ResolvedCredential {
    Password(Zeroizing<String>),
    PrivateKey(Zeroizing<Vec<u8>>),
    KeyPassphrase(Zeroizing<String>),
}

/// Host profile 按稳定身份从当前配置仓储读取。
#[async_trait]
pub trait HostConnectionRepository: Send + Sync {
    /// 按稳定主机 ID 读取连接配置，避免 UI 或运行时依赖文件路径。
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile>;
}

/// 凭据仅在使用时从加密 vault 解析，不复制到主机配置。
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// 仅在连接时从凭据库解析引用，并由 Zeroizing 包装返回的秘密。
    async fn resolve(
        &self,
        reference: &CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential>;
}

/// known_hosts 访问通过端口注入，使桌面复用应用存储、测试使用隔离状态。
#[async_trait]
pub trait KnownHostsRepository: Send + Sync {
    /// 检查当前指纹并区分未知、已知和变化，默认不得自动信任。
    async fn inspect(
        &self,
        endpoint: &HostEndpoint,
        identity: &HostKeyIdentity,
    ) -> AppResult<HostKeyStatus>;
    /// 只有经过单独确认并二次验证的指纹才可写入可信集合。
    async fn trust(&self, endpoint: &HostEndpoint, identity: &HostKeyIdentity) -> AppResult<()>;

    /// 仅在上层绑定旧指纹且再次确认新身份后原子替换指定端点的旧记录。
    async fn replace(
        &self,
        endpoint: &HostEndpoint,
        expected_previous_fingerprint_sha256: &str,
        identity: &HostKeyIdentity,
    ) -> AppResult<()>;
}

/// 桌面平台的 RDP 启动 adapter；密码只能短暂交给平台凭据/DPAPI 实现，不得写进 argv。
#[async_trait]
pub trait RdpPlatformLauncher: Send + Sync {
    /// 仅以 cancel 约束启动和资源注册；成功后平台 adapter 将进程交给应用生命周期 owner。
    async fn launch(
        &self,
        target: RdpLaunchTarget,
        cancel: CancellationToken,
    ) -> AppResult<RdpLaunchReceipt>;
}

/// 桌面或 MCP 发起 RDP 启动时只传稳定主机身份，不传 host/credential 内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RdpLaunchRequest {
    pub host_id: HostId,
}

/// 平台 adapter 的短生命周期输入；password 不实现 Debug，降低误记录泄密风险。
pub struct RdpLaunchTarget {
    pub endpoint: HostEndpoint,
    pub password: Option<Zeroizing<String>>,
    pub display: management::RdpDisplayOptions,
}

/// 仅在平台客户端确认启动后返回的安全回执。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RdpLaunchReceipt {
    /// 与状态查询/订阅对应的不透明启动身份，不暴露主机信息。
    pub launch_id: RdpLaunchId,
    pub endpoint: HostEndpoint,
    pub process_id: Option<u32>,
    /// 区分由本进程监督的客户端与交给系统打开器的客户端，避免把启动回执误当成可管理进程。
    pub lifecycle: RdpClientLifecycle,
}

/// 表示 RDP 客户端资源的真正所有者，供 UI 和退出流程选择正确的状态语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RdpClientLifecycle {
    /// Kerminal 启动并监督子进程，退出时负责停止并回收临时资源。
    ManagedProcess,
    /// 系统打开器接管客户端，Kerminal 不拥有客户端进程生命周期。
    DelegatedToSystem,
}

/// 解析 RDP host 与 vault 引用，再委托平台 launcher 安全创建客户端会话。
pub struct RdpLaunchService {
    hosts: Arc<dyn HostConnectionRepository>,
    host_management: Option<management::HostManagementService>,
    credentials: Arc<dyn CredentialResolver>,
    launcher: Arc<dyn RdpPlatformLauncher>,
}

impl RdpLaunchService {
    /// 显式注入主机目录、凭据 vault 与平台启动器，避免该用例自行访问磁盘或 keychain。
    pub fn new(
        hosts: Arc<dyn HostConnectionRepository>,
        credentials: Arc<dyn CredentialResolver>,
        launcher: Arc<dyn RdpPlatformLauncher>,
    ) -> Self {
        Self {
            hosts,
            host_management: None,
            credentials,
            launcher,
        }
    }

    /// 可选注入与主机编辑器共用的选项来源，保持既有 factory 构造调用兼容。
    ///
    /// @author kongweiguang
    pub fn with_host_management(mut self, hosts: management::HostManagementService) -> Self {
        self.host_management = Some(hosts);
        self
    }

    /// 校验目标协议并读取同一主机显示设置；只有 launcher 成功启动后才产生回执。
    ///
    /// @author kongweiguang
    pub async fn launch(
        &self,
        request: RdpLaunchRequest,
        cancel: CancellationToken,
    ) -> AppResult<RdpLaunchReceipt> {
        let profile = self.hosts.load(&request.host_id).await?;
        let display = if let Some(hosts) = &self.host_management {
            let host = hosts
                .load_host(&request.host_id)
                .await?
                .ok_or_else(|| AppError::not_found("RDP 主机配置已不存在"))?;
            if host.protocol != profile.protocol
                || host.endpoint != profile.endpoint
                || host.rdp_password != profile.rdp_password
            {
                return Err(AppError::conflict("RDP 主机配置在启动前发生变化，请重试"));
            }
            host.rdp_display
        } else {
            management::RdpDisplayOptions::default()
        };
        display.validate()?;
        if profile.protocol != RemoteProtocol::Rdp {
            return Err(AppError::capability_not_supported("RDP launch"));
        }
        if profile.endpoint.host.trim().is_empty()
            || profile.endpoint.port == 0
            || profile
                .endpoint
                .host
                .chars()
                .chain(profile.endpoint.username.chars())
                .any(char::is_control)
        {
            return Err(AppError::invalid("RDP endpoint 无效"));
        }
        let password = if let Some(reference) = &profile.rdp_password {
            match self.credentials.resolve(reference, &cancel).await? {
                ResolvedCredential::Password(password) => Some(password),
                _ => {
                    return Err(AppError::invalid(
                        "RDP password credential has the wrong type",
                    ));
                }
            }
        } else {
            None
        };
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("RDP launch was cancelled"));
        }
        self.launcher
            .launch(
                RdpLaunchTarget {
                    endpoint: profile.endpoint,
                    password,
                    display,
                },
                cancel,
            )
            .await
    }
}

/// 串口枚举与平台 API 解耦；UI 不依赖 serialport/tokio-serial 的具体类型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialPortDescriptor {
    pub name: String,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
}

/// 提供可连接串口的只读目录，枚举失败作为明确应用错误返回。
#[async_trait]
pub trait SerialPortRepository: Send + Sync {
    /// 列出当前可见串口；取消只结束本次枚举请求。
    async fn list(&self, cancel: CancellationToken) -> AppResult<Vec<SerialPortDescriptor>>;
}

/// 通过应用用例访问当前平台可用的串口目录，避免 UI 直接依赖设备驱动 crate。
#[derive(Clone)]
pub struct SerialPortService {
    repository: Arc<dyn SerialPortRepository>,
}

impl SerialPortService {
    /// 注入 composition root 选择的平台目录适配器；表单可安全共享同一只读服务。
    ///
    /// @author kongweiguang
    pub fn new(repository: Arc<dyn SerialPortRepository>) -> Self {
        Self { repository }
    }

    /// 将单次、可取消的设备枚举转交给平台目录，并避免已取消的 UI 请求继续触碰设备 API。
    ///
    /// @author kongweiguang
    pub async fn list(&self, cancel: CancellationToken) -> AppResult<Vec<SerialPortDescriptor>> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("串口设备枚举已取消"));
        }
        let ports = self.repository.list(cancel.clone()).await?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("串口设备枚举已取消"));
        }
        Ok(ports)
    }
}

/// 终端、文件与运维适配器共用的 SSH 连接端口。
#[async_trait]
pub trait RemoteConnectionService: Send + Sync {
    /// 先验证协议能力，再解析凭据和建立连接，保证 SFTP-only 目标不会意外联网启动 shell。
    async fn connect(
        &self,
        host_id: HostId,
        intent: ConnectionIntent,
        cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease>;
    /// 读取单个 target/jump peer 的当前指纹供显式确认 UI 展示。
    async fn inspect_host_key(
        &self,
        host_id: HostId,
        peer: HostKeyPeer,
        cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge>;
    /// 重新握手并比对用户确认的完整 peer/fingerprint 后才持久化信任。
    async fn trust_host_key(
        &self,
        request: HostKeyTrustRequest,
        cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt>;

    /// 重新探测 HostId 与 peer；只有旧/新端点及指纹都匹配确认时才替换 known_hosts。
    async fn replace_host_key(
        &self,
        request: HostKeyRotationRequest,
        cancel: CancellationToken,
    ) -> AppResult<HostKeyRotationReceipt>;
}

/// 已认证连接的拥有式租约；各操作在共享 transport 上打开各自的 channel。
#[derive(Clone)]
pub struct RemoteConnectionLease {
    inner: Arc<dyn RemoteConnectionHandle>,
}

impl RemoteConnectionLease {
    /// 公开构造仅供基础设施 adapter 实现应用端口，调用方应只从 service 获取 lease。
    pub fn new(inner: Arc<dyn RemoteConnectionHandle>) -> Self {
        Self { inner }
    }
    /// 返回稳定主机 ID，供终端会话和运行态资源绑定目标。
    pub fn host_id(&self) -> &HostId {
        self.inner.host_id()
    }
    /// 返回已验证的协议能力，用于 UI 与业务层保持一致判断。
    pub fn capabilities(&self) -> ConnectionCapabilities {
        self.inner.capabilities()
    }
    /// 返回 host profile 的可信 shell 方言，调用方不可覆盖它以控制命令编码。
    pub fn shell_dialect(&self) -> RemoteShellDialect {
        self.inner.shell_dialect()
    }

    /// 返回当前 lease 的 shell 实现探测结果；Posix 方言本身不足以推断 Bash。
    pub fn shell_kind(&self) -> RemoteShellKind {
        self.inner.shell_kind()
    }

    /// 返回自有 shell transport 交给 SessionService；连接层不维护第二个 PTY/session registry。
    pub async fn open_terminal(
        &self,
        request: TerminalOpenRequest,
        cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        self.require(ConnectionIntent::Terminal)?;
        self.inner.open_terminal(request, cancel).await
    }

    /// 在已认证 SSH 连接上打开独立 exec channel，避免与交互终端争用字节流。
    pub async fn exec_stream(
        &self,
        request: ExecRequest,
        cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        self.require(ConnectionIntent::Exec)?;
        self.inner.exec_stream(request, cancel).await
    }

    /// 按 SSH 本义执行调用方明确提供的原始 shell 脚本；环境变量仍由 adapter 按可信方言编码。
    pub async fn exec_shell(
        &self,
        request: ShellScriptRequest,
        cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        self.require(ConnectionIntent::Exec)?;
        self.inner.exec_shell(request, cancel).await
    }

    /// 独立 SFTP subsystem channel 自己承担背压，批量传输不阻塞终端输入。
    pub async fn open_sftp(
        &self,
        request: OpenSftpRequest,
        cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        self.require(ConnectionIntent::Sftp)?;
        self.inner.open_sftp(request, cancel).await
    }

    /// 打开 direct-tcpip 通道；监听 socket 与 SOCKS 编排留给端口转发应用服务。
    pub async fn open_tunnel(
        &self,
        request: TunnelRequest,
        cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        self.require(ConnectionIntent::Tunnel)?;
        self.inner.open_tunnel(request, cancel).await
    }

    /// 在远端申请监听口，供 Remote 与 RemoteDynamic 转发复用同一个认证连接。
    pub async fn request_remote_forward(
        &self,
        request: RemoteForwardRequest,
        cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        self.require(ConnectionIntent::Tunnel)?;
        self.inner.request_remote_forward(request, cancel).await
    }

    /// 仅由连接所有者结束共享 SSH transport；单个 channel 释放不影响同连接的其它能力。
    pub async fn close(&self, reason: &str) -> AppResult<()> {
        self.close_until(
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            reason,
        )
        .await
    }

    /// 将调用方的绝对关闭期限直接交给 transport owner，避免多租约清理时重置相对超时。
    pub async fn close_until(&self, deadline: tokio::time::Instant, reason: &str) -> AppResult<()> {
        self.inner.close_until(deadline, reason).await
    }

    /// 在操作触碰连接之前执行能力门禁，确保 SFTP-only 目标 fail closed。
    fn require(&self, intent: ConnectionIntent) -> AppResult<()> {
        let c = self.capabilities();
        let allowed = match intent {
            ConnectionIntent::Terminal => c.terminal,
            ConnectionIntent::Exec => c.exec,
            ConnectionIntent::Sftp => c.sftp,
            ConnectionIntent::Tunnel => c.tunnel,
        };
        if allowed {
            Ok(())
        } else {
            let capability = match intent {
                ConnectionIntent::Terminal => "terminal",
                ConnectionIntent::Exec => "exec",
                ConnectionIntent::Sftp => "sftp",
                ConnectionIntent::Tunnel => "tunnel",
            };
            Err(AppError::capability_not_supported(capability))
        }
    }
}

/// russh adapter 实现的动态分发端口；各操作返回自有 channel 以明确资源寿命。
#[async_trait]
pub trait RemoteConnectionHandle: Send + Sync {
    /// 返回资源归属主机，供业务调用方核对目标绑定。
    fn host_id(&self) -> &HostId;
    /// 返回服务建立连接时已经校验的目标能力。
    fn capabilities(&self) -> ConnectionCapabilities;
    /// 返回通过 host repository 解析的方言，供 operations 显示或记录执行来源。
    fn shell_dialect(&self) -> RemoteShellDialect;
    /// 返回此已认证连接上通过 nonce 应答确认的 shell 类型，无法确定时保持 Unknown。
    fn shell_kind(&self) -> RemoteShellKind {
        RemoteShellKind::Unknown
    }
    /// 打开 shell channel 并转换成共享 SessionService 使用的 transport。
    async fn open_terminal(
        &self,
        request: TerminalOpenRequest,
        cancel: CancellationToken,
    ) -> AppResult<SessionTransport>;
    /// 打开独立 exec channel；取消只关闭本次命令，不销毁共享 SSH lease。
    async fn exec_stream(
        &self,
        request: ExecRequest,
        cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle>;
    /// 执行调用方显式提供的 shell script；实现不可把它重新解释为 argv 或拼接额外用户输入。
    async fn exec_shell(
        &self,
        request: ShellScriptRequest,
        cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle>;
    /// 打开独立 SFTP subsystem channel，批量通道可与交互通道隔离。
    async fn open_sftp(
        &self,
        request: OpenSftpRequest,
        cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle>;
    /// 打开单个 direct-tcpip channel，forward listener 的所有权另由转发服务管理。
    async fn open_tunnel(
        &self,
        request: TunnelRequest,
        cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle>;
    /// 请求 SSH server 监听远端端口，接收的每条连接仍是独立 channel。
    async fn request_remote_forward(
        &self,
        request: RemoteForwardRequest,
        cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle>;
    /// 在调用方提供的统一绝对期限内关闭连接及仍附着的 channel。
    async fn close_until(&self, deadline: tokio::time::Instant, reason: &str) -> AppResult<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalOpenRequest {
    pub term: String,
    pub size: TerminalSize,
    pub environment: BTreeMap<String, String>,
    /// 人工明确指定的初始 shell 命令；结构化运维命令仍通过 `ExecRequest` 编码。
    pub command: Option<String>,
    /// 需要在交互 PTY 中启动的显式程序；与原始 shell 命令互斥。
    pub program: Option<String>,
    /// 与 `program` 配对的参数，按 profile shell 方言编码为单条 SSH exec payload。
    pub args: Vec<String>,
    /// 结构化程序启动前的工作目录。
    pub working_directory: Option<String>,
}

/// 结构化程序和参数；发送到 SSH exec wire 前仍须按目标 shell 方言正确 quoting。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: Option<String>,
}

/// 原始脚本请求仅用于明确接受 shell 语义的能力；环境值须由 adapter 安全编码。
#[derive(Clone, Eq, PartialEq)]
pub struct ShellScriptRequest {
    pub script: String,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited {
        code: Option<u32>,
        signal: Option<String>,
    },
}

#[async_trait]
pub trait RemoteCommandInput: Send + Sync {
    /// 并发写入当前命令 stdin；实现必须串行化单个 channel 的写操作并响应 close/cancel。
    async fn write_stdin(&self, bytes: Vec<u8>) -> AppResult<()>;
    /// 半关闭 stdin；重复关闭必须幂等，且不得结束 stdout/stderr 消费。
    async fn close_stdin(&self) -> AppResult<()>;
}

/// 可与命令输出 reader 并行驱动的独立输入句柄。
#[derive(Clone)]
pub struct CommandInputHandle {
    inner: Arc<dyn RemoteCommandInput>,
}

impl CommandInputHandle {
    /// 包装可并发共享的 transport writer，不暴露 russh 或 PTY 实现类型。
    pub fn new(inner: Arc<dyn RemoteCommandInput>) -> Self {
        Self { inner }
    }

    /// 向远端 stdin 写入拥有式字节，避免调用方的临时 buffer 跨异步 await 借用。
    pub async fn write_stdin(&self, bytes: impl Into<Vec<u8>>) -> AppResult<()> {
        self.inner.write_stdin(bytes.into()).await
    }

    /// 发送 stdin EOF，同时保留正在读取的输出通道。
    pub async fn close_stdin(&self) -> AppResult<()> {
        self.inner.close_stdin().await
    }
}

#[async_trait]
pub trait RemoteCommandStream: Send + Sync {
    /// 读取 stdout/stderr 或一次退出事件；字节保持原样交给调用层解码。
    async fn next_event(&mut self) -> AppResult<Option<CommandEvent>>;
    /// 返回可独立于 next_event 并发使用的 stdin writer。
    fn input_handle(&self) -> CommandInputHandle;
    /// 用统一绝对 deadline 结束本次命令 channel，超时需保留内部清理 owner 以便后续重试。
    async fn close_until(&mut self, deadline: tokio::time::Instant) -> AppResult<()>;
    /// 使用有限默认预算关闭本 channel；需要与其它资源共享预算时必须调用 close_until。
    async fn close(&mut self) -> AppResult<()> {
        self.close_until(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
            .await
    }
}

pub struct CommandStreamHandle {
    inner: Box<dyn RemoteCommandStream>,
    input: CommandInputHandle,
}
impl CommandStreamHandle {
    /// 包装 adapter stream，隐藏其具体 reader/channel 类型。
    pub fn new(inner: Box<dyn RemoteCommandStream>) -> Self {
        let input = inner.input_handle();
        Self { inner, input }
    }
    /// 读取下一条有序命令事件。
    pub async fn next_event(&mut self) -> AppResult<Option<CommandEvent>> {
        self.inner.next_event().await
    }
    /// 写入交互式 exec 子进程 stdin。
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> AppResult<()> {
        self.input.write_stdin(bytes.to_vec()).await
    }
    /// 半关闭命令 stdin，同时保持 stdout/stderr 读取能力。
    pub async fn close_stdin(&mut self) -> AppResult<()> {
        self.input.close_stdin().await
    }
    /// 克隆出可与 reader 并发使用的输入端，避免双向流动时借用同一个 handle。
    pub fn input_handle(&self) -> CommandInputHandle {
        self.input.clone()
    }
    /// 结束该命令流并释放独立 channel。
    pub async fn close(&mut self) -> AppResult<()> {
        self.inner.close().await
    }
    /// 将同一绝对关闭期限交给 adapter，避免外层 timeout 丢弃拥有读泵的关闭 future。
    pub async fn close_until(&mut self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.inner.close_until(deadline).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenSftpRequest {
    pub lane: ChannelLane,
}

pub trait RemoteSftpChannelIo: AsyncDuplex {}
impl<T: AsyncDuplex> RemoteSftpChannelIo for T {}

/// 自有 SSH SFTP subsystem 字节流；交给 SFTP client 后仍保持 SSH lease 存活。
pub struct SftpChannelHandle {
    inner: Pin<Box<dyn RemoteSftpChannelIo>>,
}
impl SftpChannelHandle {
    /// 将拥有底层 SSH session 引用的 subsystem stream 包装成高层 SFTP client 的输入。
    pub fn new(inner: Box<dyn RemoteSftpChannelIo>) -> Self {
        Self {
            inner: Box::into_pin(inner),
        }
    }
}
impl AsyncRead for SftpChannelHandle {
    /// 直接轮询底层 channel reader，保留异步背压语义。
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}
impl AsyncWrite for SftpChannelHandle {
    /// 直接轮询底层 writer，避免在传输层复制整块文件内容。
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }
    /// 将 flush 交给 channel writer，等待已排队数据被送出。
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }
    /// 结束当前 subsystem channel，不关闭 lease 内其它会话 channel。
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelRequest {
    pub destination: TunnelEndpoint,
}

/// 请求 SSH server 创建远端 listener；port 为 0 时允许服务器分配可用端口。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteForwardRequest {
    pub bind_host: String,
    pub bind_port: u16,
}

#[async_trait]
pub trait RemoteForwardListener: Send + Sync {
    /// 阻塞到下一条 forwarded-tcpip channel 到达或 token 被取消。
    async fn accept(&mut self, cancel: CancellationToken)
    -> AppResult<Option<TunnelChannelHandle>>;
    /// 撤销远端 listener，且不影响共享 SSH lease 上的其它 channel。
    async fn close(&mut self) -> AppResult<()>;
    /// 返回服务端确认的 bind endpoint，包含动态分配端口。
    fn bind_endpoint(&self) -> (&str, u16);
}

/// 可独立取消的远端 listener；所有权与连接 lease 分开，避免关闭转发时重连 SSH。
pub struct RemoteForwardHandle {
    inner: Box<dyn RemoteForwardListener>,
}
impl RemoteForwardHandle {
    /// 包装基础设施 listener，使操作服务不依赖 russh 类型。
    pub fn new(inner: Box<dyn RemoteForwardListener>) -> Self {
        Self { inner }
    }
    /// 返回远端确认的实际绑定地址。
    pub fn bind_endpoint(&self) -> (&str, u16) {
        self.inner.bind_endpoint()
    }
    /// 接收远端新连接；取消仅结束该 accept。
    pub async fn accept(
        &mut self,
        cancel: CancellationToken,
    ) -> AppResult<Option<TunnelChannelHandle>> {
        self.inner.accept(cancel).await
    }
    /// 显式撤销服务器 listener。
    pub async fn close(&mut self) -> AppResult<()> {
        self.inner.close().await
    }
}
pub trait RemoteTunnelIo: AsyncDuplex {}
impl<T: AsyncDuplex> RemoteTunnelIo for T {}

/// 自有 direct-tcpip channel；释放此值只关闭当前隧道，不关闭同一 lease 下的其它通道。
pub struct TunnelChannelHandle {
    inner: Pin<Box<dyn RemoteTunnelIo>>,
}
impl TunnelChannelHandle {
    /// 包装单个隧道 stream，令 reader/writer 与该 channel 共用一个 drop 生命周期。
    pub fn new(inner: Box<dyn RemoteTunnelIo>) -> Self {
        Self {
            inner: Box::into_pin(inner),
        }
    }
}
impl AsyncRead for TunnelChannelHandle {
    /// 读取该隧道接收数据，保留 Tokio poll/backpressure 契约。
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}
impl AsyncWrite for TunnelChannelHandle {
    /// 将数据写入目标 TCP 转发 channel。
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }
    /// 刷新转发数据，避免把拷贝缓冲无界堆积在应用层。
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }
    /// 关闭当前隧道方向，释放本地数据通道。
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}
