//! 连接域模型：描述连接目标与协议能力，不持有任何运行态凭据。
//!
//! @author kongweiguang

pub mod credentials;

use std::{fmt, path::PathBuf};

use crate::identity::HostId;

/// 远程主机协议。SFTP 仍走 SSH transport，但权限边界没有 shell 能力。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteProtocol {
    Ssh,
    Sftp,
    Telnet,
    Serial,
    Rdp,
}

/// 协议对应的可执行能力，供应用层在联网前拒绝越权请求。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionCapabilities {
    pub terminal: bool,
    pub exec: bool,
    pub sftp: bool,
    pub tunnel: bool,
    pub launch_rdp: bool,
}

impl ConnectionCapabilities {
    /// 保持纯 SFTP 主机不能被误用作 shell 主机，能力由协议唯一推导。
    pub const fn for_protocol(protocol: RemoteProtocol) -> Self {
        match protocol {
            RemoteProtocol::Ssh => Self {
                terminal: true,
                exec: true,
                sftp: true,
                tunnel: true,
                launch_rdp: false,
            },
            RemoteProtocol::Sftp => Self {
                terminal: false,
                exec: false,
                sftp: true,
                tunnel: false,
                launch_rdp: false,
            },
            RemoteProtocol::Telnet | RemoteProtocol::Serial => Self {
                terminal: true,
                exec: false,
                sftp: false,
                tunnel: false,
                launch_rdp: false,
            },
            RemoteProtocol::Rdp => Self {
                terminal: false,
                exec: false,
                sftp: false,
                tunnel: false,
                launch_rdp: true,
            },
        }
    }
}

/// SSH channel 的资源隔离类别；bulk 传输使用独立 channel，避免阻塞交互输入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelLane {
    Interactive,
    Exec,
    Bulk,
    Tunnel,
}

/// 远端 exec 前必须明确平台 shell 方言，避免把结构化参数未经转义拼成命令。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteShellDialect {
    Posix,
    Fish,
    PowerShell,
    Unknown,
}

/// 不含认证材料的网络端点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostEndpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
}

impl HostEndpoint {
    /// 拒绝控制字符与空字段，避免连接标识和协议握手出现歧义。
    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() || self.username.trim().is_empty() || self.port == 0 {
            return Err("host, username and nonzero port are required".to_owned());
        }
        if self
            .host
            .chars()
            .chain(self.username.chars())
            .any(char::is_control)
        {
            return Err("host and username cannot contain control characters".to_owned());
        }
        Ok(())
    }
}

/// Vault 引用只用于解析凭据；Debug 有意隐藏引用本身，避免日志暴露 vault 索引。
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// 只接受非空、无控制字符的 opaque 引用，不在领域层解释 vault 格式。
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err("credential reference is invalid".to_owned());
        }
        Ok(Self(value))
    }

    /// 返回给 vault adapter 使用的 opaque token；调用方不得将该值写入普通日志。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CredentialRef {
    /// 隐藏 vault 索引，防止诊断输出泄露关联关系。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialRef([redacted])")
    }
}

/// SSH 认证类型与凭据位置；实际秘密只能由 vault/agent adapter 在运行时解析。
#[derive(Clone, Eq, PartialEq)]
pub enum SshAuthentication {
    Password {
        secret: CredentialRef,
    },
    PrivateKey {
        path: Option<PathBuf>,
        secret: Option<CredentialRef>,
        passphrase: Option<CredentialRef>,
    },
    Agent,
}

impl fmt::Debug for SshAuthentication {
    /// 隐藏私钥路径和所有凭据引用，避免连接 profile 被诊断日志展开。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password { .. } => formatter.write_str("Password([redacted])"),
            Self::PrivateKey { .. } => formatter.write_str("PrivateKey([redacted])"),
            Self::Agent => formatter.write_str("Agent"),
        }
    }
}

/// HTTP CONNECT 或 SOCKS5 代理配置，不内嵌密码。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyProfile {
    pub protocol: ProxyProtocol,
    pub endpoint: HostEndpoint,
    pub password: Option<CredentialRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyProtocol {
    HttpConnect,
    Socks5,
}

/// 一条跳板上的 SSH endpoint 与凭据引用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshJumpHost {
    pub endpoint: HostEndpoint,
    pub authentication: SshAuthentication,
}

/// 显式网络路径，顺序为 proxy → jump_hosts[0..n] → target。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SshRoute {
    pub proxy: Option<ProxyProfile>,
    pub jump_hosts: Vec<SshJumpHost>,
}

/// 经 repository 解析的连接目标。该值只有配置与引用，不能保存秘密。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostConnectionProfile {
    pub host_id: HostId,
    pub protocol: RemoteProtocol,
    pub endpoint: HostEndpoint,
    pub authentication: Option<SshAuthentication>,
    /// RDP 密码只保留 vault 引用；运行时 launcher 才能短暂解析明文秘密。
    pub rdp_password: Option<CredentialRef>,
    pub route: SshRoute,
    pub shell_dialect: RemoteShellDialect,
    pub connect_timeout_ms: u64,
    pub keepalive_interval_ms: Option<u64>,
}

/// TCP endpoint for a local/remote tunnel; hostnames remain data, never shell text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelEndpoint {
    pub host: String,
    pub port: u16,
}
