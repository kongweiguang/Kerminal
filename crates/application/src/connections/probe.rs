//! 未保存主机草稿的短生命周期连接探测用例。
//!
//! @author kongweiguang

use std::{collections::HashSet, fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol, RemoteShellDialect,
        SshAuthentication, SshJumpHost, SshRoute,
    },
    files::RemotePath,
    sessions::SerialSessionTarget,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    HostKeyPeer,
    management::{HostAuthenticationMode, HostCredentialChange, HostCredentialSlot, HostDraft},
};
use crate::{AppError, AppResult};

/// 单次探测的超时边界，避免未保存草稿占用后台任务过久。
pub const MIN_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// 应用层探测端口；协议 socket、known_hosts 与设备操作留在基础设施适配器。
#[async_trait]
pub trait ConnectionProbeBackend: Send + Sync {
    /// 对已经校验和归一化的草稿执行一次有界探测，不得持久化 profile 或临时凭据。
    async fn probe(
        &self,
        target: ConnectionProbeTarget,
        cancellation: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome>;
}

/// 未保存连接的输入。凭据变更只用于本次探测，不能传给主机配置或 vault 写入用例。
pub struct ConnectionProbeRequest {
    pub draft: HostDraft,
    pub credential_changes: Vec<HostCredentialChange>,
    pub timeout: Duration,
    pub host_key_confirmation: Option<ProbeHostKeyConfirmation>,
}

/// 对未知 host key 的一次性确认，精确绑定到 peer、endpoint 和 SHA-256 指纹。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeHostKeyConfirmation {
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub fingerprint_sha256: String,
}

/// 已知 key 不匹配时返回的安全挑战摘要，不带公钥正文或任何认证材料。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeHostKeyChallenge {
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub algorithm: String,
    pub fingerprint_sha256: String,
}

/// host key 已变化时保留新旧摘要，便于 UI 指向正式信任管理流程。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeHostKeyChanged {
    pub peer: HostKeyPeer,
    pub endpoint: HostEndpoint,
    pub algorithm: String,
    pub previous_fingerprint_sha256: String,
    pub fingerprint_sha256: String,
}

/// 本机可用的 RDP 客户端；不表达远端连通性或认证状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RdpClientKind {
    WindowsMstsc,
    LinuxFreeRdp,
}

/// 本机 RDP 启动能力与远端 TCP 探测分别记录，避免一项结果冒充另一项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RdpLocalAvailability {
    ClientAvailable(RdpClientKind),
    SystemLauncherAvailable { association_verified: bool },
    Unavailable,
}

/// TCP 建连错误只分类传输层现象，不把端口可达描述为 RDP 握手或认证成功。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RdpTcpFailure {
    ConnectionRefused,
    TimedOut,
    NetworkError,
}

/// 远端 TCP 层的实际探测结果；超时和取消仍通过应用错误单独返回。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RdpTcpReachability {
    Reachable,
    Unreachable(RdpTcpFailure),
}

/// 一次 RDP 测试中独立采集的本机启动能力和远端 TCP 结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RdpConnectivityProbe {
    pub local_availability: RdpLocalAvailability,
    pub remote_tcp: RdpTcpReachability,
    pub remote_port: u16,
}

/// 探测结果刻意区分认证/子系统、Telnet/串口传输和 RDP 本机/远端测试。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionProbeOutcome {
    Verified(ProbeVerificationLevel),
    RdpConnectivity(RdpConnectivityProbe),
    RequiresHostKeyTrust(ProbeHostKeyChallenge),
    HostKeyChanged(ProbeHostKeyChanged),
}

/// 可以证明的连接层级，不把 Telnet TCP 或串口打开误报为远端登录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeVerificationLevel {
    SshAuthenticated,
    SftpSubsystemReady,
    SftpDirectoryAccessible,
    TelnetTransportConnected,
    SerialDeviceOpened,
}

/// 短生命周期运行计划；凭据引用只指向本次内存 resolver 或原有 vault。
pub struct ConnectionProbeTarget {
    pub profile: HostConnectionProfile,
    /// SFTP 探测的只读起始目录；SFTP 未配置时按 UI 默认根目录 `/` 探测，其它协议为 None。
    pub sftp_remote_start_directory: Option<RemotePath>,
    pub serial: Option<SerialSessionTarget>,
    pub timeout: Duration,
    pub host_key_confirmation: Option<ProbeHostKeyConfirmation>,
    pub transient_credentials: Vec<ProbeCredential>,
}

/// 本次 UI 输入的零化凭据材料，不允许以 Debug/错误格式展开正文。
pub struct ProbeCredential {
    pub reference: CredentialRef,
    pub material: ProbeCredentialMaterial,
}

/// 短生命周期凭据类型，按协议所需形状交给 credential resolver。
pub enum ProbeCredentialMaterial {
    Password(Zeroizing<String>),
    PrivateKey(Zeroizing<Vec<u8>>),
    KeyPassphrase(Zeroizing<String>),
}

impl fmt::Debug for ProbeCredential {
    /// 日志仅显示材料类型和脱敏引用，绝不输出用户刚输入的秘密。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let material = match &self.material {
            ProbeCredentialMaterial::Password(_) => "Password([redacted])",
            ProbeCredentialMaterial::PrivateKey(_) => "PrivateKey([redacted])",
            ProbeCredentialMaterial::KeyPassphrase(_) => "KeyPassphrase([redacted])",
        };
        formatter
            .debug_struct("ProbeCredential")
            .field("reference", &self.reference)
            .field("material", &material)
            .finish()
    }
}

/// 以显式 backend 注入创建 probe 用例，避免应用层自行获取网络、设备或文件系统能力。
pub struct ConnectionProbeService {
    backend: Arc<dyn ConnectionProbeBackend>,
}

impl ConnectionProbeService {
    /// 只绑定一个执行端口，确保未保存草稿不绕过统一基础设施连接实现。
    pub fn new(backend: Arc<dyn ConnectionProbeBackend>) -> Self {
        Self { backend }
    }

    /// 校验草稿并调用单次探测端口；取消在执行前即返回，不触碰设备或网络。
    pub async fn probe(
        &self,
        request: ConnectionProbeRequest,
        cancellation: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("连接测试已取消"));
        }
        let target = build_probe_target(request)?;
        self.backend.probe(target, cancellation).await
    }
}

/// 把 UI 草稿转换为隔离 profile，并将秘密转成仅本次 resolver 可识别的随机引用。
/// SFTP 起始目录在应用边界复用保存用例的约束，并归一化为领域 RemotePath 后交给 adapter。
///
/// @author kongweiguang
fn build_probe_target(mut request: ConnectionProbeRequest) -> AppResult<ConnectionProbeTarget> {
    validate_probe_input(
        &request.draft,
        request.timeout,
        request.host_key_confirmation.as_ref(),
    )?;
    let host_id = request.draft.id.take().unwrap_or_default();
    let transient_credentials =
        apply_transient_credentials(&mut request.draft, request.credential_changes)?;

    let protocol = request.draft.protocol;
    let authentication = if matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
        Some(resolve_authentication(
            request.draft.authentication.take(),
            request.draft.authentication_mode,
        )?)
    } else {
        None
    };
    let jump_hosts = if matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
        request
            .draft
            .route
            .jump_hosts
            .into_iter()
            .map(|jump| {
                Ok(SshJumpHost {
                    endpoint: jump.endpoint,
                    authentication: resolve_authentication(
                        jump.authentication,
                        jump.authentication_mode,
                    )?,
                })
            })
            .collect::<AppResult<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let route = if matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
        SshRoute {
            proxy: request.draft.route.proxy,
            jump_hosts,
        }
    } else {
        SshRoute::default()
    };
    let serial = if protocol == RemoteProtocol::Serial {
        request.draft.serial
    } else {
        None
    };
    let sftp_remote_start_directory = if protocol == RemoteProtocol::Sftp {
        let directory = request
            .draft
            .sftp_options
            .remote_start_directory
            .as_deref()
            .unwrap_or("/");
        Some(RemotePath::parse(directory).map_err(|_| AppError::invalid("SFTP 起始目录无效"))?)
    } else {
        None
    };
    let profile = HostConnectionProfile {
        host_id,
        protocol,
        endpoint: request.draft.endpoint,
        authentication,
        // RDP 探测只建立 TCP 连接，不进行协议认证，因此不解析或传递远端口令。
        rdp_password: None,
        route,
        shell_dialect: RemoteShellDialect::Unknown,
        connect_timeout_ms: request.timeout.as_millis().min(u64::MAX as u128) as u64,
        keepalive_interval_ms: None,
    };
    Ok(ConnectionProbeTarget {
        profile,
        sftp_remote_start_directory,
        serial,
        timeout: request.timeout,
        host_key_confirmation: request.host_key_confirmation,
        transient_credentials,
    })
}

/// 仅验证连接测试所需字段，避免调用主机保存路径或复用其持久化副作用。
/// 保持 probe 与保存用例对远端起始目录采用同一绝对 POSIX 路径约束。
///
/// @author kongweiguang
fn validate_probe_input(
    draft: &HostDraft,
    timeout: Duration,
    confirmation: Option<&ProbeHostKeyConfirmation>,
) -> AppResult<()> {
    if !(MIN_PROBE_TIMEOUT..=MAX_PROBE_TIMEOUT).contains(&timeout) {
        return Err(AppError::invalid("连接测试超时需在 1 到 120 秒之间"));
    }
    match draft.protocol {
        RemoteProtocol::Ssh | RemoteProtocol::Sftp => {
            draft
                .endpoint
                .validate()
                .map_err(|_| AppError::invalid("SSH/SFTP 地址、端口或用户名无效"))?;
            if draft.serial.is_some() {
                return Err(AppError::invalid("网络主机不能携带串口目标"));
            }
            validate_route(draft)?;
        }
        RemoteProtocol::Telnet => {
            validate_network_endpoint(&draft.endpoint, true)?;
            if draft.serial.is_some()
                || draft.authentication.is_some()
                || draft.rdp_password.is_some()
                || !draft.route.jump_hosts.is_empty()
                || draft.route.proxy.is_some()
            {
                return Err(AppError::invalid("Telnet 草稿包含不支持的连接配置"));
            }
        }
        RemoteProtocol::Serial => {
            let serial = draft
                .serial
                .as_ref()
                .ok_or_else(|| AppError::invalid("串口连接参数不完整"))?;
            if serial.device.as_os_str().is_empty() || serial.baud_rate == 0 {
                return Err(AppError::invalid("串口设备或波特率无效"));
            }
            if draft.authentication.is_some()
                || draft.rdp_password.is_some()
                || draft.route.proxy.is_some()
                || !draft.route.jump_hosts.is_empty()
            {
                return Err(AppError::invalid("串口草稿包含不支持的认证或路由配置"));
            }
        }
        RemoteProtocol::Rdp => {
            if draft.endpoint.host.trim().is_empty()
                || draft
                    .endpoint
                    .host
                    .chars()
                    .any(|ch| ch.is_whitespace() || ch.is_control())
                || draft.endpoint.port == 0
                || draft.endpoint.username.chars().any(char::is_control)
            {
                return Err(AppError::invalid("RDP 地址、端口或用户名无效"));
            }
            if draft.authentication.is_some()
                || draft.serial.is_some()
                || draft.route.proxy.is_some()
                || !draft.route.jump_hosts.is_empty()
            {
                return Err(AppError::invalid("RDP 草稿包含不支持的认证或路由配置"));
            }
        }
    }
    if draft
        .sftp_options
        .remote_start_directory
        .as_deref()
        .is_some_and(|path| !super::management::valid_remote_start_directory(path))
    {
        return Err(AppError::invalid("SFTP 起始目录无效"));
    }
    if let Some(confirmation) = confirmation
        && (confirmation.fingerprint_sha256.trim().is_empty()
            || confirmation
                .fingerprint_sha256
                .chars()
                .any(char::is_control)
            || !matches!(draft.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp))
    {
        return Err(AppError::invalid("主机指纹确认不适用于当前目标"));
    }
    Ok(())
}

/// 确保代理和每个跳板均属于 SSH/SFTP 草稿，并在拨号前拒绝无效 endpoint。
fn validate_route(draft: &HostDraft) -> AppResult<()> {
    if let Some(proxy) = &draft.route.proxy {
        validate_network_endpoint(&proxy.endpoint, false)?;
        if proxy.password.is_some() && proxy.endpoint.username.trim().is_empty() {
            return Err(AppError::invalid("带密码的代理需要用户名"));
        }
    }
    for jump in &draft.route.jump_hosts {
        jump.endpoint
            .validate()
            .map_err(|_| AppError::invalid("SSH 跳板地址、端口或用户名无效"))?;
    }
    Ok(())
}

/// 校验无需领域用户名的网络 endpoint，同时约束地址和端口不会进入命令解释路径。
fn validate_network_endpoint(endpoint: &HostEndpoint, require_port: bool) -> AppResult<()> {
    if endpoint.host.trim().is_empty()
        || endpoint.host.chars().any(|ch| ch.is_control())
        || (require_port && endpoint.port == 0)
    {
        return Err(AppError::invalid("网络地址或端口无效"));
    }
    Ok(())
}

/// 将编辑表单的一次性秘密挂到随机凭据引用；清除只影响副本，不写配置或 vault。
fn apply_transient_credentials(
    draft: &mut HostDraft,
    changes: Vec<HostCredentialChange>,
) -> AppResult<Vec<ProbeCredential>> {
    let mut seen = HashSet::new();
    let mut credentials = Vec::new();
    for change in changes {
        let (slot, replacement) = match change {
            HostCredentialChange::Replace { slot, secret } => (slot, Some(secret)),
            HostCredentialChange::Clear { slot } => (slot, None),
        };
        if !seen.insert(slot) {
            return Err(AppError::invalid("同一凭据字段不能重复提交"));
        }
        validate_credential_slot(draft, slot)?;
        let reference = match replacement {
            Some(secret) if slot == HostCredentialSlot::RdpPassword => {
                // RDP 连通性测试不执行认证，不把刚输入的口令送入客户端或临时文件。
                drop(secret.into_zeroizing());
                None
            }
            Some(secret) => {
                let secret = secret.into_zeroizing();
                if secret.trim().is_empty() {
                    return Err(AppError::invalid("凭据不能为空"));
                }
                let reference = CredentialRef::new(format!("probe-{}", Uuid::new_v4().simple()))
                    .map_err(|_| AppError::invalid("无法生成临时凭据引用"))?;
                let material = transient_material(slot, secret);
                credentials.push(ProbeCredential {
                    reference: reference.clone(),
                    material,
                });
                Some(reference)
            }
            None => None,
        };
        set_credential_reference(draft, slot, reference)?;
    }
    Ok(credentials)
}

/// 按表单槽位校验凭据材料类别，防止例如跳板口令误绑定到目标或 RDP。
fn validate_credential_slot(draft: &HostDraft, slot: HostCredentialSlot) -> AppResult<()> {
    let ssh = matches!(draft.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp);
    let valid =
        match slot {
            HostCredentialSlot::RdpPassword => draft.protocol == RemoteProtocol::Rdp,
            HostCredentialSlot::ProxyPassword => ssh && draft.route.proxy.is_some(),
            HostCredentialSlot::TargetPassword => {
                ssh && draft.authentication_mode == HostAuthenticationMode::Password
            }
            HostCredentialSlot::TargetPrivateKey | HostCredentialSlot::TargetKeyPassphrase => {
                ssh && draft.authentication_mode == HostAuthenticationMode::PrivateKey
            }
            HostCredentialSlot::JumpPassword(index) => {
                ssh && draft.route.jump_hosts.get(index).is_some_and(|jump| {
                    jump.authentication_mode == HostAuthenticationMode::Password
                })
            }
            HostCredentialSlot::JumpPrivateKey(index)
            | HostCredentialSlot::JumpKeyPassphrase(index) => {
                ssh && draft.route.jump_hosts.get(index).is_some_and(|jump| {
                    jump.authentication_mode == HostAuthenticationMode::PrivateKey
                })
            }
        };
    if valid {
        Ok(())
    } else {
        Err(AppError::invalid("凭据字段与本次连接草稿不匹配"))
    }
}

/// 把一次性字符串转换为适配器所需的零化凭据形状。
fn transient_material(
    slot: HostCredentialSlot,
    secret: Zeroizing<String>,
) -> ProbeCredentialMaterial {
    match slot {
        HostCredentialSlot::TargetPrivateKey | HostCredentialSlot::JumpPrivateKey(_) => {
            ProbeCredentialMaterial::PrivateKey(Zeroizing::new(secret.as_bytes().to_vec()))
        }
        HostCredentialSlot::TargetKeyPassphrase | HostCredentialSlot::JumpKeyPassphrase(_) => {
            ProbeCredentialMaterial::KeyPassphrase(secret)
        }
        _ => ProbeCredentialMaterial::Password(secret),
    }
}

/// 将替换或清除应用到草稿副本，不调用 vault 或配置仓储。
fn set_credential_reference(
    draft: &mut HostDraft,
    slot: HostCredentialSlot,
    reference: Option<CredentialRef>,
) -> AppResult<()> {
    match slot {
        HostCredentialSlot::RdpPassword => draft.rdp_password = reference,
        HostCredentialSlot::ProxyPassword => {
            draft
                .route
                .proxy
                .as_mut()
                .ok_or_else(|| AppError::invalid("代理凭据没有对应的代理配置"))?
                .password = reference;
        }
        HostCredentialSlot::TargetPassword => {
            draft.authentication = reference.map(|secret| SshAuthentication::Password { secret });
        }
        HostCredentialSlot::TargetPrivateKey | HostCredentialSlot::TargetKeyPassphrase => {
            let (path, mut secret, mut passphrase) = take_private_key(draft.authentication.take())?;
            if slot == HostCredentialSlot::TargetPrivateKey {
                secret = reference;
            } else {
                passphrase = reference;
            }
            draft.authentication = Some(SshAuthentication::PrivateKey {
                path,
                secret,
                passphrase,
            });
        }
        HostCredentialSlot::JumpPassword(index)
        | HostCredentialSlot::JumpPrivateKey(index)
        | HostCredentialSlot::JumpKeyPassphrase(index) => {
            let jump = draft
                .route
                .jump_hosts
                .get_mut(index)
                .ok_or_else(|| AppError::not_found("凭据所属跳板不存在"))?;
            if matches!(slot, HostCredentialSlot::JumpPassword(_)) {
                jump.authentication =
                    reference.map(|secret| SshAuthentication::Password { secret });
            } else {
                let (path, mut secret, mut passphrase) =
                    take_private_key(jump.authentication.take())?;
                if matches!(slot, HostCredentialSlot::JumpPrivateKey(_)) {
                    secret = reference;
                } else {
                    passphrase = reference;
                }
                jump.authentication = Some(SshAuthentication::PrivateKey {
                    path,
                    secret,
                    passphrase,
                });
            }
        }
    }
    Ok(())
}

/// 拆出可复制的私钥来源，同时拒绝在口令认证模式下误改私钥字段。
fn take_private_key(
    authentication: Option<SshAuthentication>,
) -> AppResult<(
    Option<std::path::PathBuf>,
    Option<CredentialRef>,
    Option<CredentialRef>,
)> {
    match authentication {
        Some(SshAuthentication::PrivateKey {
            path,
            secret,
            passphrase,
        }) => Ok((path, secret, passphrase)),
        None => Ok((None, None, None)),
        _ => Err(AppError::invalid("私钥凭据字段与认证模式不匹配")),
    }
}

/// 将表单认证模式落为运行时必需的 SSH authentication；缺少认证材料时在拨号前拒绝。
fn resolve_authentication(
    authentication: Option<SshAuthentication>,
    mode: HostAuthenticationMode,
) -> AppResult<SshAuthentication> {
    match (mode, authentication) {
        (HostAuthenticationMode::Agent, None | Some(SshAuthentication::Agent)) => {
            Ok(SshAuthentication::Agent)
        }
        (
            HostAuthenticationMode::Password,
            Some(authentication @ SshAuthentication::Password { .. }),
        ) => Ok(authentication),
        (
            HostAuthenticationMode::PrivateKey,
            Some(authentication @ SshAuthentication::PrivateKey { .. }),
        ) => {
            let SshAuthentication::PrivateKey { path, secret, .. } = &authentication else {
                return Err(AppError::invalid("SSH 私钥认证模式与草稿凭据不匹配"));
            };
            if path.is_none() && secret.is_none() {
                return Err(AppError::invalid("SSH 私钥认证尚未提供私钥文件或内容"));
            }
            Ok(authentication)
        }
        _ => Err(AppError::invalid("SSH 认证模式与草稿凭据不匹配")),
    }
}

#[cfg(test)]
#[path = "../../tests/support/connections_probe.rs"]
mod tests;
