//! 本地与远程隧道的配置和生命周期。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};
use std::{fmt, net::IpAddr, num::NonZeroU16};

use crate::identity::HostId;

/// 端口转发类型对应 SSH 的四种不同监听/连接方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ForwardKind {
    Local,
    Remote,
    Dynamic,
    RemoteDynamic,
}

/// 远端监听的请求范围；实际可访问地址始终以服务端确认的 bind endpoint 为准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteAccessScope {
    Loopback,
    PrivateNetwork,
    AllInterfaces,
    Custom,
}

/// 动态 SOCKS5 转发的请求应用范围；该值只描述调用意图，不代表已注入目标环境。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProxyApplyScope {
    #[default]
    None,
    CurrentTerminal,
    FutureTerminals,
    UserProfile,
    ToolOnly,
}

/// 当前支持的动态代理协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProxyProtocol {
    Socks5,
}

/// 代理应用未发生时可供调用方修正下一步动作的明确原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProxyApplicationReason {
    TunnelOnly,
    CallerMustApplyProxyUrl,
    NoProxyEndpoint,
    NoTerminalTarget,
    NoFutureTerminalIntegration,
    NoExplicitProfileTarget,
}

/// 隧道状态和代理应用状态分开返回，避免把已启动转发误报为代理已注入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyApplicationOutcome {
    pub scope: ProxyApplyScope,
    pub applied: bool,
    pub reason: ProxyApplicationReason,
}

impl ProxyApplicationOutcome {
    /// 根据请求范围构造未应用结果，当前用例不修改终端环境或用户 profile。
    pub const fn not_applied(scope: ProxyApplyScope, has_proxy_endpoint: bool) -> Self {
        let reason = match scope {
            ProxyApplyScope::None => ProxyApplicationReason::TunnelOnly,
            ProxyApplyScope::ToolOnly if has_proxy_endpoint => {
                ProxyApplicationReason::CallerMustApplyProxyUrl
            }
            ProxyApplyScope::ToolOnly => ProxyApplicationReason::NoProxyEndpoint,
            ProxyApplyScope::CurrentTerminal => ProxyApplicationReason::NoTerminalTarget,
            ProxyApplyScope::FutureTerminals => ProxyApplicationReason::NoFutureTerminalIntegration,
            ProxyApplyScope::UserProfile => ProxyApplicationReason::NoExplicitProfileTarget,
        };
        Self {
            scope,
            applied: false,
            reason,
        }
    }
}

/// 绑定或目的端点；端口必须已经过非零解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardEndpoint {
    pub host: String,
    pub port: NonZeroU16,
}

impl ForwardEndpoint {
    /// 规范化 IP 字面量，主机名保持原样以支持远端 DNS。
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, PortForwardTransitionError> {
        let host = host.into().trim().to_owned();
        let port = NonZeroU16::new(port).ok_or(PortForwardTransitionError::InvalidPort)?;
        if host.is_empty() || host.chars().any(char::is_control) {
            return Err(PortForwardTransitionError::InvalidHost);
        }
        let host = host
            .parse::<IpAddr>()
            .map_or(host.clone(), |ip| ip.to_string());
        Ok(Self { host, port })
    }
}

impl fmt::Display for ForwardEndpoint {
    /// IPv6 地址加括号以避免 host:port 文本歧义。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') && !self.host.starts_with('[') {
            write!(formatter, "[{}]:{}", self.host, self.port)
        } else {
            write!(formatter, "{}:{}", self.host, self.port)
        }
    }
}

/// 用户请求的持久化配置；运行时 handle 不进入此类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortForwardConfig {
    pub host_id: HostId,
    pub name: String,
    pub kind: ForwardKind,
    pub bind: ForwardEndpoint,
    pub destination: Option<ForwardEndpoint>,
    /// 原 MCP 协议的动态转发协议；没有显式提供时由动态转发默认使用 SOCKS5。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_protocol: Option<ProxyProtocol>,
    /// 保留调用方声明的远端监听范围；监听实际地址由 bind 和 actual endpoint 决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_access_scope: Option<RemoteAccessScope>,
    /// 保留代理应用意图；任何未应用的范围都会在运行结果中明确说明。
    #[serde(default)]
    pub proxy_apply_scope: ProxyApplyScope,
}

impl PortForwardConfig {
    /// 确保 SOCKS 类型没有固定目的地，普通转发则必须带有目的端点。
    pub fn validate(&self) -> Result<(), PortForwardTransitionError> {
        let dynamic = matches!(self.kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
        if dynamic == self.destination.is_some() {
            return Err(PortForwardTransitionError::DestinationMismatch);
        }
        if self.name.trim().is_empty() || self.name.chars().any(char::is_control) {
            return Err(PortForwardTransitionError::InvalidName);
        }
        Ok(())
    }
}

/// 运行期状态；关闭后不可回到活动状态，重新启动创建新的 runtime handle。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardState {
    Starting,
    Active,
    Closing,
    Stopped,
    Failed,
}

/// 有状态端口转发实例。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortForward {
    pub id: String,
    pub config: PortForwardConfig,
    pub state: ForwardState,
}

impl PortForward {
    /// 创建实例时先标记 Starting，只有底层隧道已建立才激活。
    pub fn starting(id: impl Into<String>, config: PortForwardConfig) -> Self {
        Self {
            id: id.into(),
            config,
            state: ForwardState::Starting,
        }
    }

    /// 只允许从 Starting 进入 Active，重复激活说明所有权已错乱。
    pub fn activate(&mut self) -> Result<(), PortForwardTransitionError> {
        self.transition(ForwardState::Active, &[ForwardState::Starting])
    }

    /// 关闭前显式进入 Closing，便于 UI 显示资源释放中的状态。
    pub fn begin_close(&mut self) -> Result<(), PortForwardTransitionError> {
        self.transition(
            ForwardState::Closing,
            &[
                ForwardState::Starting,
                ForwardState::Active,
                ForwardState::Failed,
            ],
        )
    }

    /// 完成 close 后固定为 Stopped，重复关闭按幂等成功处理。
    pub fn stop(&mut self) -> Result<(), PortForwardTransitionError> {
        if self.state == ForwardState::Stopped {
            return Ok(());
        }
        self.transition(ForwardState::Stopped, &[ForwardState::Closing])
    }

    /// 连接中断也可能发生在活动期，因此允许运行态转发进入 Failed 并由调用方释放资源。
    pub fn fail(&mut self) -> Result<(), PortForwardTransitionError> {
        self.transition(
            ForwardState::Failed,
            &[
                ForwardState::Starting,
                ForwardState::Active,
                ForwardState::Closing,
            ],
        )
    }

    /// 统一检查状态转换，保持隧道 handle 与业务状态同步。
    fn transition(
        &mut self,
        next: ForwardState,
        allowed: &[ForwardState],
    ) -> Result<(), PortForwardTransitionError> {
        if !allowed.contains(&self.state) {
            return Err(PortForwardTransitionError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }
}

/// 端口转发领域约束错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortForwardTransitionError {
    InvalidPort,
    InvalidHost,
    InvalidName,
    DestinationMismatch,
    InvalidTransition {
        from: ForwardState,
        to: ForwardState,
    },
}
