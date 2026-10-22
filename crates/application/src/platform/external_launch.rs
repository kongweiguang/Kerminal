//! 外部协议和第三方客户端的安全路由用例。
//!
//! 解析只产生类型化意图；实际连接由工作区显式发起，未知输入不会触发网络副作用。
//!
//! @author kongweiguang

mod parser;

use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use crate::preferences::settings::{ExternalLaunchSettings, ExternalLaunchToolSetting};
use async_trait::async_trait;
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
};
use zeroize::Zeroizing;

use crate::{AppError, AppResult};

/// 把首次启动参数和单实例转发统一收敛成有界数据，避免宿主为两种入口各写解析器。
#[derive(Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalActivation {
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
}

impl ExternalActivation {
    /// 在单实例 IPC 反序列化后、进入解析器前限制资源消耗并拒绝 NUL 注入。
    pub fn validate(&self) -> AppResult<()> {
        const MAX_ARGUMENTS: usize = 128;
        const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
        const MAX_TOTAL_BYTES: usize = 64 * 1024;
        const MAX_DIRECTORY_BYTES: usize = 4096;

        if self.arguments.len() > MAX_ARGUMENTS
            || self
                .arguments
                .iter()
                .any(|argument| argument.len() > MAX_ARGUMENT_BYTES || argument.contains('\0'))
            || self.working_directory.as_ref().is_some_and(|directory| {
                directory.len() > MAX_DIRECTORY_BYTES || directory.contains('\0')
            })
        {
            return Err(AppError::invalid("外部启动参数超出安全限制"));
        }

        let total_bytes = self.arguments.iter().map(String::len).sum::<usize>()
            + self.working_directory.as_ref().map_or(0, String::len);
        if total_bytes > MAX_TOTAL_BYTES {
            return Err(AppError::invalid("外部启动参数超出安全限制"));
        }
        Ok(())
    }
}

impl fmt::Debug for ExternalActivation {
    /// argv 和工作目录可能包含凭据或用户路径，诊断输出只保留其结构信息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalActivation")
            .field("argument_count", &self.arguments.len())
            .field(
                "working_directory_present",
                &self.working_directory.is_some(),
            )
            .finish()
    }
}

/// 识别出的来源仅用于显示标签和兼容既有外部客户端入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalLaunchSource {
    Kerminal,
    OpenSsh,
    Putty,
    MobaXterm,
    Xshell,
    SecureCrt,
    SftpClient,
}

/// 跳板路由身份与目标连接参数；此结构不含认证秘密。
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalTargetIdentity {
    pub protocol: RemoteProtocol,
    pub endpoint: HostEndpoint,
    pub jump_hosts: Vec<HostEndpoint>,
}

impl fmt::Debug for ExternalTargetIdentity {
    /// 主机和用户名属于用户连接上下文，避免被普通调试日志展开。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalTargetIdentity")
            .field("protocol", &self.protocol)
            .field("jump_count", &self.jump_hosts.len())
            .field("endpoint", &"[redacted]")
            .finish()
    }
}

/// 应用层提交给短期连接仓储的认证方式提示，不包含任何秘密材料。
#[derive(Clone, Eq, PartialEq)]
pub enum ExternalAuthenticationHint {
    Agent,
    PrivateKey { path: PathBuf },
    PasswordPrompt,
}

impl fmt::Debug for ExternalAuthenticationHint {
    /// 私钥路径会暴露用户目录，普通诊断仅展示认证方式。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => formatter.write_str("Agent"),
            Self::PrivateKey { .. } => formatter.write_str("PrivateKey([path omitted])"),
            Self::PasswordPrompt => formatter.write_str("PasswordPrompt"),
        }
    }
}

/// 未保存的目标只能存在于运行时内存，配置仓储不得将其写入 host TOML。
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalTemporaryTargetSpec {
    pub identity: ExternalTargetIdentity,
    pub display_name: Option<String>,
    pub authentication: ExternalAuthenticationHint,
}

impl fmt::Debug for ExternalTemporaryTargetSpec {
    /// 隔离显示名和用户路径，避免外部 URL 内容出现在结构化日志。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalTemporaryTargetSpec")
            .field("protocol", &self.identity.protocol)
            .field("authentication", &self.authentication)
            .field("display_name_present", &self.display_name.is_some())
            .finish()
    }
}

/// 连接目标的只读查找端口；命中仅用于显示与旧版 Known 安全分类。
#[async_trait]
pub trait SavedExternalTargetLookup: Send + Sync + 'static {
    /// 按完整 endpoint 和有序跳板链匹配保存目标，不返回或读取其 Vault 凭据。
    async fn find_exact(&self, target: &ExternalTargetIdentity) -> AppResult<Option<HostId>>;
}

/// 为临时连接的每个消费者持有独立生命周期；release 必须短小、同步且不可失败。
pub trait ExternalTargetLeaseOwner: Send + Sync + 'static {
    /// 在目标仍有效时为一个独立消费者增加引用计数，退出清理后拒绝继续 retain。
    fn retain_scope(&self, host_id: &HostId) -> AppResult<()>;

    /// 释放一个消费者 scope；实现只执行内存状态更新，不访问磁盘或网络。
    fn release_scope(&self, host_id: &HostId);
}

struct ExternalTargetLeaseScope {
    host_id: HostId,
    owner: Arc<dyn ExternalTargetLeaseOwner>,
}

impl Drop for ExternalTargetLeaseScope {
    /// 最后一个 lease clone 消失时同步通知仓储，确保密码和 profile 不因遗忘显式清理而残留。
    fn drop(&mut self) {
        self.owner.release_scope(&self.host_id);
    }
}

/// 一个临时连接消费者的 RAII 所有权；普通 Clone 共享 scope，retain 创建独立 scope。
#[derive(Clone)]
pub struct ExternalTargetLease {
    scope: Arc<ExternalTargetLeaseScope>,
}

impl ExternalTargetLease {
    /// 注册成功时由基础设施创建初始 scope；非法 ID 会立即释放注册计数。
    pub fn new_registered(
        host_id: HostId,
        owner: Arc<dyn ExternalTargetLeaseOwner>,
    ) -> AppResult<Self> {
        if !host_id.as_str().starts_with("external:") || host_id.as_str().len() <= 9 {
            owner.release_scope(&host_id);
            return Err(AppError::infrastructure("临时外部目标标识无效"));
        }
        Ok(Self {
            scope: Arc::new(ExternalTargetLeaseScope { host_id, owner }),
        })
    }

    /// 从 lease 读取临时 host 标识，避免调用者另存一份可能脱离所有权的身份。
    pub fn host_id(&self) -> &HostId {
        &self.scope.host_id
    }

    /// 为另一个终端或文件面板创建独立 owner scope，避免一个消费者关闭时误删其他消费者的目标。
    pub fn retain(&self) -> AppResult<Self> {
        self.scope.owner.retain_scope(&self.scope.host_id)?;
        Ok(Self {
            scope: Arc::new(ExternalTargetLeaseScope {
                host_id: self.scope.host_id.clone(),
                owner: self.scope.owner.clone(),
            }),
        })
    }
}

impl fmt::Debug for ExternalTargetLease {
    /// Debug 仅展示不含认证材料的 opaque ID，避免打印目标 profile 或凭据。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalTargetLease")
            .field("host_id", self.host_id())
            .finish()
    }
}

impl PartialEq for ExternalTargetLease {
    /// 生命周期 scope 不参与业务相等性，临时目标以 host ID 标识。
    fn eq(&self, other: &Self) -> bool {
        self.host_id() == other.host_id()
    }
}

impl Eq for ExternalTargetLease {}

/// 外部临时目标的内存生命周期端口，由连接基础设施提供组合仓储实现。
#[async_trait]
pub trait ExternalTemporaryTargetRegistry: ExternalTargetLeaseOwner {
    /// 创建进程内 profile 和初始 lease，禁止写入 host TOML 或持久化历史。
    async fn register(&self, target: ExternalTemporaryTargetSpec)
    -> AppResult<ExternalTargetLease>;

    /// 将用户在 Kerminal 提示框输入的密码短暂放入内存凭据解析器。
    async fn set_password(&self, host_id: &HostId, password: Zeroizing<String>) -> AppResult<()>;

    /// 进程退出时强制清空全部目标和凭据；后续旧 lease 的释放按失效目标处理。
    async fn clear_all(&self) -> AppResult<()>;
}

/// 目标字段尚不完整时保留已解析的意图，等待 UI 明确补齐用户名。
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalLaunchRequest {
    pub source: ExternalLaunchSource,
    pub target: ExternalTargetDraft,
    pub intent: ExternalLaunchIntent,
    pub display_name: Option<String>,
    pub identity_file: Option<PathBuf>,
}

impl fmt::Debug for ExternalLaunchRequest {
    /// 外部请求中的命令、主机名和本机密钥路径不应进入默认 Debug 输出。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalLaunchRequest")
            .field("source", &self.source)
            .field("protocol", &self.target.protocol)
            .field("username_present", &self.target.username.is_some())
            .field("identity_file_present", &self.identity_file.is_some())
            .field("intent", &self.intent)
            .finish()
    }
}

/// 尚未补齐用户名的目标草稿；只有 route 完成验证后才能转为连接身份。
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalTargetDraft {
    pub protocol: RemoteProtocol,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub jump_hosts: Vec<ExternalJumpHostDraft>,
}

impl fmt::Debug for ExternalTargetDraft {
    /// Draft 可能来自任意外部链接，普通 Debug 只显示字段是否齐全。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalTargetDraft")
            .field("protocol", &self.protocol)
            .field("host_present", &!self.host.is_empty())
            .field("port", &self.port)
            .field("username_present", &self.username.is_some())
            .field("jump_count", &self.jump_hosts.len())
            .finish()
    }
}

/// 一个跳板 endpoint 草稿；缺省用户名会在目标用户名确定后继承。
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalJumpHostDraft {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
}

impl fmt::Debug for ExternalJumpHostDraft {
    /// 跳板登录标识可能泄露内部拓扑，Debug 不输出具体连接字段。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalJumpHostDraft")
            .field("host_present", &!self.host.is_empty())
            .field("port", &self.port)
            .field("username_present", &self.username.is_some())
            .finish()
    }
}

/// 外部调用要求的工作区动作；远程命令保持 argv 结构，禁止拼接成 shell 文本。
#[derive(Clone, Eq, PartialEq)]
pub enum ExternalLaunchIntent {
    SshTerminal {
        remote_command: Vec<String>,
    },
    SftpTransfer {
        remote_path: String,
        expected_host_key: Option<String>,
    },
}

impl fmt::Debug for ExternalLaunchIntent {
    /// 远程命令和 SFTP 路径由不可信进程提供，日志只记录动作种类与参数数量。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SshTerminal { remote_command } => formatter
                .debug_struct("SshTerminal")
                .field("remote_command_argument_count", &remote_command.len())
                .finish(),
            Self::SftpTransfer {
                expected_host_key, ..
            } => formatter
                .debug_struct("SftpTransfer")
                .field("expected_host_key_present", &expected_host_key.is_some())
                .field("remote_path", &"[redacted]")
                .finish(),
        }
    }
}

/// 精确匹配结果仅说明目标与保存配置相同，不代表授权使用其中的认证凭据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalTargetClassification {
    Known { saved_host_id: HostId },
    RestrictedUnknown,
    LookupUnavailable,
}

/// 已注册的本次外部目标，可由 UI 路由到终端或 SFTP 工作区。
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalRoutedTarget {
    pub lease: ExternalTargetLease,
    pub identity: ExternalTargetIdentity,
    pub classification: ExternalTargetClassification,
    pub intent: ExternalLaunchIntent,
    pub display_name: Option<String>,
    pub authentication_required: bool,
    /// SSH 连接成功后额外打开同一目标的 SFTP；不改变 SSH 的主意图。
    pub open_sftp_after_connect: bool,
}

impl ExternalRoutedTarget {
    /// 提供兼容便捷读取；消费者持有本结构时同时持有对应 owner scope。
    pub fn host_id(&self) -> &HostId {
        self.lease.host_id()
    }
}

impl fmt::Debug for ExternalRoutedTarget {
    /// 路由结果携带用户主机信息，诊断仅显示安全分类和会话动作。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalRoutedTarget")
            .field("lease", &self.lease)
            .field("classification", &self.classification)
            .field("authentication_required", &self.authentication_required)
            .field("open_sftp_after_connect", &self.open_sftp_after_connect)
            .field("intent", &self.intent)
            .finish()
    }
}

/// 外部输入解析后的路由结果；所有分支均由 UI 明确处理。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalLaunchOutcome {
    Ignored,
    UsernameRequired { request: ExternalLaunchRequest },
    PasswordRequired { target: ExternalRoutedTarget },
    Ready { target: ExternalRoutedTarget },
}

/// 协调白名单解析、保存目标安全分类及临时连接目标注册。
#[derive(Clone)]
pub struct ExternalLaunchService {
    saved_targets: Arc<dyn SavedExternalTargetLookup>,
    temporary_targets: Arc<dyn ExternalTemporaryTargetRegistry>,
    settings: Arc<RwLock<ExternalLaunchSettings>>,
}

impl ExternalLaunchService {
    /// 通过明确依赖注入查找、临时目标和初始策略，避免业务服务自行读取 UI 或磁盘设置。
    pub fn new(
        saved_targets: Arc<dyn SavedExternalTargetLookup>,
        temporary_targets: Arc<dyn ExternalTemporaryTargetRegistry>,
        settings: ExternalLaunchSettings,
    ) -> Self {
        Self {
            saved_targets,
            temporary_targets,
            settings: Arc::new(RwLock::new(settings)),
        }
    }

    /// 设置保存后更新共享策略快照，使下一次启动请求立即按新开关执行。
    pub fn update_settings(&self, settings: ExternalLaunchSettings) -> AppResult<()> {
        *self
            .settings
            .write()
            .map_err(|_| AppError::infrastructure("外部启动策略不可用"))? = settings;
        Ok(())
    }

    /// 先执行全局和来源策略，再运行白名单解析；普通或被禁用参数不会创建连接目标。
    pub async fn handle_activation(
        &self,
        activation: ExternalActivation,
    ) -> AppResult<ExternalLaunchOutcome> {
        self.ensure_launch_enabled()?;
        activation.validate()?;
        if let Some(source) = parser::source_hint(&activation) {
            self.ensure_source_enabled(source)?;
        }
        let Some(request) = parser::parse_activation(&activation)? else {
            return Ok(ExternalLaunchOutcome::Ignored);
        };
        self.ensure_source_enabled(request.source)?;
        self.route_request(request).await
    }

    /// 用户补齐用户名后复用同一请求解析结果，不重新解释或拼接原始 argv。
    pub async fn supply_username(
        &self,
        mut request: ExternalLaunchRequest,
        username: String,
    ) -> AppResult<ExternalLaunchOutcome> {
        let username = username.trim();
        if username.is_empty() || username.chars().any(char::is_control) {
            return Err(AppError::invalid("外部连接用户名无效"));
        }
        request.target.username = Some(username.to_owned());
        self.route_request(request).await
    }

    /// 将已解析请求物化为一次性 host；精确保存匹配只影响安全分类，不继承其 Vault 凭据。
    pub async fn route_request(
        &self,
        request: ExternalLaunchRequest,
    ) -> AppResult<ExternalLaunchOutcome> {
        self.ensure_source_enabled(request.source)?;
        let Some(username) = request
            .target
            .username
            .as_deref()
            .map(str::trim)
            .filter(|username| !username.is_empty())
        else {
            return Ok(ExternalLaunchOutcome::UsernameRequired { request });
        };

        let identity = build_identity(&request.target, username)?;
        let classification = match self.saved_targets.find_exact(&identity).await {
            Ok(Some(saved_host_id)) => ExternalTargetClassification::Known { saved_host_id },
            Ok(None) => ExternalTargetClassification::RestrictedUnknown,
            Err(_) => ExternalTargetClassification::LookupUnavailable,
        };
        // 查找可能等待磁盘或缓存；再次读取策略可阻止等待期间被禁用的请求创建临时目标。
        self.ensure_source_enabled(request.source)?;
        let open_sftp_after_connect = self.open_sftp_after_connect(&request.intent)?;
        let authentication = authentication_hint(&request);
        let authentication_required = authentication == ExternalAuthenticationHint::PasswordPrompt;
        let lease = self
            .temporary_targets
            .register(ExternalTemporaryTargetSpec {
                identity: identity.clone(),
                display_name: request.display_name.clone(),
                authentication,
            })
            .await?;
        if !lease.host_id().as_str().starts_with("external:") {
            drop(lease);
            return Err(AppError::infrastructure("临时外部目标标识无效"));
        }

        let target = ExternalRoutedTarget {
            lease,
            identity,
            classification,
            intent: request.intent,
            display_name: request.display_name,
            authentication_required,
            open_sftp_after_connect,
        };
        if authentication_required {
            Ok(ExternalLaunchOutcome::PasswordRequired { target })
        } else {
            Ok(ExternalLaunchOutcome::Ready { target })
        }
    }

    /// 只允许把 UI 密码提示输入交给本次临时 profile，凭据不进入 argv 或持久化配置。
    pub async fn provide_password(
        &self,
        lease: &ExternalTargetLease,
        password: String,
    ) -> AppResult<()> {
        let password = Zeroizing::new(password);
        let host_id = lease.host_id();
        if !host_id.as_str().starts_with("external:") {
            return Err(AppError::invalid("外部认证只允许用于临时连接"));
        }
        if password.is_empty() || password.chars().any(char::is_control) {
            return Err(AppError::invalid("外部连接密码无效"));
        }
        self.temporary_targets.set_password(host_id, password).await
    }

    /// 进程退出时清空暂存 profile 和零化密码；普通会话关闭依靠各自 lease 自动回收。
    pub async fn clear_temporary_targets_on_shutdown(&self) -> AppResult<()> {
        self.temporary_targets.clear_all().await
    }

    /// 服务关闭时在参数解析前拒绝一切外部启动，确保开关关闭没有连接副作用。
    fn ensure_launch_enabled(&self) -> AppResult<()> {
        let settings = self
            .settings
            .read()
            .map_err(|_| AppError::infrastructure("外部启动策略不可用"))?;
        if settings.enabled {
            Ok(())
        } else {
            Err(AppError::unsupported("外部启动已在设置中禁用"))
        }
    }

    /// 对已识别的来源应用客户端白名单与动态禁用表，拒绝路径不调用查找或临时目标端口。
    fn ensure_source_enabled(&self, source: ExternalLaunchSource) -> AppResult<()> {
        let settings = self
            .settings
            .read()
            .map_err(|_| AppError::infrastructure("外部启动策略不可用"))?;
        if !settings.enabled {
            return Err(AppError::unsupported("外部启动已在设置中禁用"));
        }
        if source != ExternalLaunchSource::Kerminal && !settings.accept_vendor_args {
            return Err(AppError::unsupported("第三方客户端参数接收已在设置中禁用"));
        }
        if settings
            .disabled_tools
            .contains(&external_tool_setting(source))
        {
            return Err(AppError::unsupported("此外部客户端已在设置中禁用"));
        }
        Ok(())
    }

    /// 把旧版自动打开偏好保留为 SSH 成功后的附加动作，不覆盖 SFTP 或 SSH 主意图。
    fn open_sftp_after_connect(&self, intent: &ExternalLaunchIntent) -> AppResult<bool> {
        let settings = self
            .settings
            .read()
            .map_err(|_| AppError::infrastructure("外部启动策略不可用"))?;
        Ok(matches!(intent, ExternalLaunchIntent::SshTerminal { .. }) && settings.auto_open_sftp)
    }
}

/// 把解析器来源映射到偏好中的稳定客户端 ID，避免两处各自维护字符串协议。
fn external_tool_setting(source: ExternalLaunchSource) -> ExternalLaunchToolSetting {
    match source {
        ExternalLaunchSource::Kerminal => ExternalLaunchToolSetting::KerminalNative,
        ExternalLaunchSource::OpenSsh => ExternalLaunchToolSetting::Openssh,
        ExternalLaunchSource::Putty => ExternalLaunchToolSetting::Putty,
        ExternalLaunchSource::MobaXterm => ExternalLaunchToolSetting::Mobaxterm,
        ExternalLaunchSource::Xshell => ExternalLaunchToolSetting::Xshell,
        ExternalLaunchSource::SecureCrt => ExternalLaunchToolSetting::Securecrt,
        ExternalLaunchSource::SftpClient => ExternalLaunchToolSetting::SftpClient,
    }
}

/// 把目标草稿转为完整身份，并为每个未指定用户名的跳板继承最终目标用户名。
fn build_identity(
    target: &ExternalTargetDraft,
    username: &str,
) -> AppResult<ExternalTargetIdentity> {
    let endpoint = HostEndpoint {
        host: canonical_host(&target.host),
        port: target.port,
        username: username.to_owned(),
    };
    endpoint
        .validate()
        .map_err(|_| AppError::invalid("外部连接目标无效"))?;

    let mut jump_hosts = Vec::with_capacity(target.jump_hosts.len());
    for jump in &target.jump_hosts {
        let endpoint = HostEndpoint {
            host: canonical_host(&jump.host),
            port: jump.port,
            username: jump
                .username
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(username)
                .to_owned(),
        };
        endpoint
            .validate()
            .map_err(|_| AppError::invalid("外部跳板目标无效"))?;
        jump_hosts.push(endpoint);
    }
    Ok(ExternalTargetIdentity {
        protocol: target.protocol,
        endpoint,
        jump_hosts,
    })
}

/// 只应用旧版安全匹配所需的主机名等价规则，不重写用户实际输入或做 DNS 查询。
fn canonical_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// 缺省认证方式沿用外部 SSH agent；SFTP 无 key 时显式要求 UI 询问密码。
fn authentication_hint(request: &ExternalLaunchRequest) -> ExternalAuthenticationHint {
    if let Some(path) = &request.identity_file {
        ExternalAuthenticationHint::PrivateKey { path: path.clone() }
    } else if request.target.protocol == RemoteProtocol::Sftp {
        ExternalAuthenticationHint::PasswordPrompt
    } else {
        ExternalAuthenticationHint::Agent
    }
}
