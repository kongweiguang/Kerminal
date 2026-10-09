//! 外部 Agent 会话文件化数据模型。
//!
//! @author kongweiguang

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[path = "agent_session/agent_id.rs"]
mod agent_id;
#[path = "agent_session/launcher_key.rs"]
mod launcher_key;
#[path = "agent_session/validation.rs"]
mod validation;

pub use self::agent_id::{AgentId, PI_AGENT_LAUNCH_COMMAND, PI_AGENT_RESUME_COMMAND};
pub use self::launcher_key::normalize_agent_launcher_key;
use self::validation::{
    is_valid_agent_session_id, normalize_optional_text, serialize_global_agent_scope,
};

/// Agent session 文件 schema 版本。
pub const AGENT_SESSION_SCHEMA_VERSION: u32 = 1;

/// Kerminal 自有的 Agent 会话主键。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct AgentSessionId(String);

impl AgentSessionId {
    /// 创建并校验 Agent 会话 id。
    pub fn new(value: impl Into<String>) -> AppResult<Self> {
        let value = value.into();
        if is_valid_agent_session_id(&value) {
            Ok(Self(value))
        } else {
            Err(AppError::InvalidInput(format!(
                "Agent session id 不合法: {value}"
            )))
        }
    }

    /// 返回字符串形式的会话 id。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for AgentSessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AgentSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Agent 会话生命周期状态。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentSessionStatus {
    /// 当前可继续使用。
    Active,
    /// 已归档，不再默认出现在 active 恢复入口。
    Archived,
    /// 会话存在，但绑定的运行态目标已经不可确认。
    Stale,
}

/// Agent CLI 启动配置。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentSessionLaunch {
    /// 用户可见命令标签，例如 `codex`。
    pub command_label: String,
    /// 实际启动的 shell 或 CLI。
    pub shell: String,
    /// 传给 shell/CLI 的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 进程工作目录，默认是该 Agent 会话目录。
    pub cwd: String,
}

/// Agent CLI 启动配置更新请求。
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionLaunchRequest {
    /// 用户可见命令标签。
    #[serde(default)]
    pub command_label: Option<String>,
    /// 实际启动的 shell 或 CLI。
    #[serde(default)]
    pub shell: Option<String>,
    /// 传给 shell/CLI 的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 进程工作目录。
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Agent 目标终端绑定的 live 状态。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentTargetLiveStatus {
    /// 尚未绑定目标终端。
    Unbound,
    /// 绑定的目标仍可解析。
    Ready,
    /// 绑定存在，但运行态目标丢失或已过期。
    Stale,
    /// 目标已关闭。
    Closed,
}

/// Agent 会话的右栏归属兼容形状。
///
/// Tab 标识隔离多个右栏助手的上下文和历史；终端工具通过
/// [`AgentSession::effective_scope`] 将所有会话统一投影为 global，当前 target
/// 只作为无显式 sessionId 时的默认首选终端。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AgentSessionScope {
    /// 右栏助手归属的当前 Tab；不限制终端工具授权。
    Tab {
        /// wire 使用 camelCase；alias 保证已落盘的早期 snake_case scope 可继续恢复。
        #[serde(rename = "tabId", alias = "tab_id")]
        tab_id: String,
    },
    /// 整个 Kerminal 工作区的所有用户终端。
    Global,
}

impl AgentSessionScope {
    /// 从旧版 target 迁移 scope，明确把无绑定 session 视为 global。
    pub fn from_legacy_target(target: Option<&AgentSessionTarget>) -> Self {
        let Some(target) = target else {
            return Self::Global;
        };
        if target.live_status == AgentTargetLiveStatus::Unbound {
            return Self::Global;
        }
        target
            .tab_id
            .as_deref()
            .map(str::trim)
            .filter(|tab_id| !tab_id.is_empty())
            .map(|tab_id| Self::Tab {
                tab_id: tab_id.to_owned(),
            })
            .unwrap_or(Self::Global)
    }

    /// 返回 tab scope 的 tab id，global scope 返回 None。
    pub fn tab_id(&self) -> Option<&str> {
        match self {
            Self::Tab { tab_id } => Some(tab_id.as_str()),
            Self::Global => None,
        }
    }

    /// 禁止空 Tab 标识，避免会话落入永远无法命中成员的伪作用域。
    pub fn validate(&self) -> AppResult<()> {
        if matches!(self, Self::Tab { tab_id } if tab_id.trim().is_empty()) {
            return Err(AppError::InvalidInput(
                "Agent tab scope 的 tabId 不能为空".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Agent 会话绑定的目标终端元数据。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentSessionTarget {
    /// 目标绑定 id。
    #[serde(default)]
    pub binding_id: Option<String>,
    /// 目标绑定 generation，用于避免旧绑定写入新终端。
    #[serde(default)]
    pub binding_generation: u64,
    /// 前端 pane id。
    #[serde(default)]
    pub pane_id: Option<String>,
    /// 前端 tab id。
    #[serde(default)]
    pub tab_id: Option<String>,
    /// 被 Agent 处理的目标终端 session id。
    #[serde(default)]
    pub target_terminal_session_id: Option<String>,
    /// 目标引用，例如 `ssh:prod-web-01`。
    #[serde(default)]
    pub target_ref: Option<String>,
    /// 目标类型，例如 `local` 或 `ssh`。
    #[serde(default)]
    pub target_kind: Option<String>,
    /// 目标终端工作目录。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 目标终端 shell。
    #[serde(default)]
    pub shell: Option<String>,
    /// 目标 live 状态。
    pub live_status: AgentTargetLiveStatus,
    /// 最近一次确认该绑定的时间。
    #[serde(default)]
    pub last_seen_at: Option<String>,
}

impl AgentSessionTarget {
    /// 返回未绑定目标。
    pub fn unbound() -> Self {
        Self {
            binding_id: None,
            binding_generation: 0,
            pane_id: None,
            tab_id: None,
            target_terminal_session_id: None,
            target_ref: None,
            target_kind: None,
            cwd: None,
            shell: None,
            live_status: AgentTargetLiveStatus::Unbound,
            last_seen_at: None,
        }
    }

    /// 当前目标是否已经 stale。
    pub fn is_stale(&self) -> bool {
        matches!(
            self.live_status,
            AgentTargetLiveStatus::Stale | AgentTargetLiveStatus::Closed
        )
    }
}

/// Agent 会话主元数据，对应 `session.toml`。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentSession {
    /// 文件 schema 版本。
    pub schema_version: u32,
    /// Kerminal Agent 会话主键。
    pub agent_session_id: AgentSessionId,
    /// 外部 Agent 类型。
    pub agent_id: AgentId,
    /// 选择器中的稳定启动身份；旧 session 缺失时由调用方按 agent/命令快照推导。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_key: Option<String>,
    /// 用户可见标题。
    pub title: String,
    /// 创建时间，使用调用方提供的稳定字符串。
    pub created_at: String,
    /// 更新时间，使用调用方提供的稳定字符串。
    pub updated_at: String,
    /// 会话生命周期状态。
    pub status: AgentSessionStatus,
    /// Kerminal 全局文件优先配置目录。
    pub workspace_root: String,
    /// 当前 Agent 会话目录。
    pub session_root: String,
    /// Agent CLI 启动信息。
    pub launch: AgentSessionLaunch,
    /// 右栏会话归属；读取旧文件时仍可按 target 推断，运行态权限另见 effective_scope。
    #[serde(default)]
    pub scope: Option<AgentSessionScope>,
    /// 当前绑定目标；未绑定时为空。
    #[serde(default)]
    pub target: Option<AgentSessionTarget>,
}

impl AgentSession {
    /// 计算终端工具的有效 scope；持久化 Tab 只隔离右栏会话，实际 Agent 能力
    /// 始终覆盖整个 Kerminal，target 仍单独承担无显式 sessionId 时的首选目标。
    pub fn effective_scope(&self) -> AgentSessionScope {
        AgentSessionScope::Global
    }

    /// 校验持久化 session 元数据。
    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(AppError::InvalidInput(format!(
                "unsupported agent session schema_version: {}, expected {}",
                self.schema_version, AGENT_SESSION_SCHEMA_VERSION
            )));
        }
        AgentSessionId::new(self.agent_session_id.as_str().to_owned())?;
        normalize_agent_launcher_key(self.agent_id, self.launcher_key.clone())?;
        if self.title.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Agent session title 不能为空".to_owned(),
            ));
        }
        if self.session_root.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Agent session root 不能为空".to_owned(),
            ));
        }
        if self.workspace_root.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Agent workspace root 不能为空".to_owned(),
            ));
        }
        if let Some(scope) = &self.scope {
            scope.validate()?;
        }
        Ok(())
    }
}

/// Provider 类型，对应 `provider.toml`。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentProvider {
    /// OpenAI Codex CLI。
    Codex,
    /// Claude Code CLI。
    Claude,
    /// PI coding agent CLI。
    Pi,
    /// 用户自定义命令。
    Custom,
}

impl From<AgentId> for AgentProvider {
    fn from(agent_id: AgentId) -> Self {
        match agent_id {
            AgentId::Codex => Self::Codex,
            AgentId::Claude => Self::Claude,
            AgentId::Pi => Self::Pi,
            AgentId::Custom => Self::Custom,
        }
    }
}

/// Provider 恢复元数据，对应 `provider.toml`。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentProviderSession {
    /// 文件 schema 版本。
    pub schema_version: u32,
    /// Provider 类型。
    pub provider: AgentProvider,
    /// Provider 自己的会话 id；只能辅助恢复，不能作为 Kerminal 主键。
    #[serde(default)]
    pub provider_session_id: Option<String>,
    /// Provider 自己的恢复命令。
    #[serde(default)]
    pub resume_command: Option<String>,
    /// Provider 是否支持恢复。
    pub resume_supported: bool,
    /// 最近一次恢复时间。
    #[serde(default)]
    pub last_resume_at: Option<String>,
}

impl AgentProviderSession {
    /// 构造默认 provider 元数据。
    pub fn for_agent(agent_id: AgentId) -> Self {
        let provider = AgentProvider::from(agent_id);
        let (resume_supported, resume_command) = match provider {
            AgentProvider::Codex => (true, Some("codex resume --last".to_owned())),
            // Claude scopes its persisted conversations by working directory. Every
            // managed session has a stable session_root, so `--continue` resumes the
            // latest conversation created in this Kerminal session instead of opening
            // a new transcript after an app restart or an explicit "继续上次" action.
            AgentProvider::Claude => (true, Some("claude --continue".to_owned())),
            // Pi discovers native MCP config from the stable session cwd; keeping the
            // default command free of legacy adapter flags lets that project config load.
            AgentProvider::Pi => (true, Some(PI_AGENT_RESUME_COMMAND.to_owned())),
            AgentProvider::Custom => (false, None),
        };
        Self {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            provider,
            provider_session_id: None,
            resume_command,
            resume_supported,
            last_resume_at: None,
        }
    }

    /// 校验 provider 文件。
    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(AppError::InvalidInput(format!(
                "unsupported agent provider schema_version: {}, expected {}",
                self.schema_version, AGENT_SESSION_SCHEMA_VERSION
            )));
        }
        Ok(())
    }
}

/// context 目录内各文件的绝对路径。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionContextPaths {
    /// `context/target-binding.json`。
    pub target_binding_json: String,
    /// `context/mcp-endpoint.json`。
    pub mcp_endpoint_json: String,
    /// `context/terminal-snapshot.json`。
    pub terminal_snapshot_json: String,
    /// `context/workspace-snapshot.json`。
    pub workspace_snapshot_json: String,
}

/// Agent session 目录与关键文件路径。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionPaths {
    /// Kerminal 全局文件优先配置目录。
    pub workspace_root: String,
    /// 当前 Agent 会话目录。
    pub session_root: String,
    /// `session.toml`。
    pub session_toml: String,
    /// `provider.toml`。
    pub provider_toml: String,
    /// `context/*` 路径集合。
    pub context: AgentSessionContextPaths,
}

/// `context/target-binding.json` 内的目标绑定视图。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTargetBindingContext {
    /// 文件 schema 版本。
    pub schema_version: u32,
    /// Kerminal Agent 会话主键。
    pub agent_session_id: AgentSessionId,
    /// 当前会话的终端操作范围兼容字段；写出时规范化为 global。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_global_agent_scope"
    )]
    pub scope: Option<AgentSessionScope>,
    /// 目标绑定详情。
    pub binding: AgentTargetBindingContextBinding,
    /// 右栏 Agent 终端详情。
    #[serde(default)]
    pub agent_terminal: Option<AgentTerminalContext>,
    /// 文件生成时间。
    pub generated_at: String,
}

impl AgentTargetBindingContext {
    /// 从 session 目标生成 context 文件内容。
    pub fn from_session_target(session: &AgentSession, generated_at: impl Into<String>) -> Self {
        Self {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id: session.agent_session_id.clone(),
            scope: Some(session.effective_scope()),
            binding: AgentTargetBindingContextBinding::from_target(session.target.as_ref()),
            agent_terminal: None,
            generated_at: generated_at.into(),
        }
    }

    /// 校验 context 文件。
    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(AppError::InvalidInput(format!(
                "unsupported target binding schemaVersion: {}, expected {}",
                self.schema_version, AGENT_SESSION_SCHEMA_VERSION
            )));
        }
        AgentSessionId::new(self.agent_session_id.as_str().to_owned())?;
        if let Some(scope) = &self.scope {
            scope.validate()?;
        }
        Ok(())
    }

    /// 兼容读取旧 target-binding.json；scope 字段继续保留以便旧客户端读取，
    /// 但 Agent 终端能力统一为 global，目标 binding 仍作为默认首选终端。
    pub fn effective_scope(&self) -> AgentSessionScope {
        AgentSessionScope::Global
    }
}

/// `target-binding.json` 的绑定状态。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentTargetBindingStatus {
    /// 未绑定。
    Unbound,
    /// 已解析。
    Ready,
    /// 已失效。
    Stale,
    /// 已关闭。
    Closed,
}

impl From<AgentTargetLiveStatus> for AgentTargetBindingStatus {
    fn from(status: AgentTargetLiveStatus) -> Self {
        match status {
            AgentTargetLiveStatus::Unbound => Self::Unbound,
            AgentTargetLiveStatus::Ready => Self::Ready,
            AgentTargetLiveStatus::Stale => Self::Stale,
            AgentTargetLiveStatus::Closed => Self::Closed,
        }
    }
}

/// `target-binding.json` 的目标绑定详情。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTargetBindingContextBinding {
    /// 目标绑定 id。
    #[serde(default)]
    pub binding_id: Option<String>,
    /// 目标绑定 generation。
    pub generation: u64,
    /// 目标绑定状态。
    pub status: AgentTargetBindingStatus,
    /// 目标是否已失效。
    pub stale: bool,
    /// 前端 pane id。
    #[serde(default)]
    pub pane_id: Option<String>,
    /// 前端 tab id。
    #[serde(default)]
    pub tab_id: Option<String>,
    /// 目标终端 session id。
    #[serde(default)]
    pub target_terminal_session_id: Option<String>,
    /// 目标引用。
    #[serde(default)]
    pub target_ref: Option<String>,
    /// 目标 cwd。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 目标 shell。
    #[serde(default)]
    pub shell: Option<String>,
}

impl AgentTargetBindingContextBinding {
    fn from_target(target: Option<&AgentSessionTarget>) -> Self {
        match target {
            Some(target) => Self {
                binding_id: target.binding_id.clone(),
                generation: target.binding_generation,
                status: AgentTargetBindingStatus::from(target.live_status),
                stale: target.is_stale(),
                pane_id: target.pane_id.clone(),
                tab_id: target.tab_id.clone(),
                target_terminal_session_id: target.target_terminal_session_id.clone(),
                target_ref: target.target_ref.clone(),
                cwd: target.cwd.clone(),
                shell: target.shell.clone(),
            },
            None => Self {
                binding_id: None,
                generation: 0,
                status: AgentTargetBindingStatus::Unbound,
                stale: false,
                pane_id: None,
                tab_id: None,
                target_terminal_session_id: None,
                target_ref: None,
                cwd: None,
                shell: None,
            },
        }
    }
}

/// 右栏 Agent terminal 的运行态摘要。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTerminalContext {
    /// 右栏 Agent terminal session id。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 运行状态，例如 `running`。
    pub status: String,
}

/// MCP 传输类型。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum AgentMcpTransport {
    /// Streamable HTTP MCP。
    #[serde(rename = "streamable-http")]
    StreamableHttp,
}

/// `context/mcp-endpoint.json` 内容。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpEndpointContext {
    /// 文件 schema 版本。
    pub schema_version: u32,
    /// Kerminal Agent 会话主键。
    pub agent_session_id: AgentSessionId,
    /// session-scoped MCP endpoint；MCP 未启动时可以为空。
    #[serde(default)]
    pub endpoint: Option<String>,
    /// MCP transport。
    pub transport: AgentMcpTransport,
    /// 是否只暴露 tools。
    pub tools_only: bool,
    /// 文件生成时间。
    pub generated_at: String,
}

impl AgentMcpEndpointContext {
    /// 创建基础 endpoint context。
    pub fn new(
        agent_session_id: AgentSessionId,
        endpoint: Option<String>,
        generated_at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id,
            endpoint: endpoint.and_then(normalize_optional_text),
            transport: AgentMcpTransport::StreamableHttp,
            tools_only: true,
            generated_at: generated_at.into(),
        }
    }

    /// 校验 endpoint context 文件。
    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(AppError::InvalidInput(format!(
                "unsupported MCP endpoint schemaVersion: {}, expected {}",
                self.schema_version, AGENT_SESSION_SCHEMA_VERSION
            )));
        }
        AgentSessionId::new(self.agent_session_id.as_str().to_owned())?;
        Ok(())
    }
}

/// `context/terminal-snapshot.json` 内容。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTerminalSnapshotContext {
    /// 文件 schema 版本。
    pub schema_version: u32,
    /// Kerminal Agent 会话主键。
    pub agent_session_id: AgentSessionId,
    /// 被快照的目标终端 session id。
    #[serde(default)]
    pub target_terminal_session_id: Option<String>,
    /// 捕获的输出字节数，按写入文件的脱敏文本计算。
    pub captured_bytes: usize,
    /// 本次快照上限。
    pub max_bytes: usize,
    /// 是否因上限发生截断。
    pub truncated: bool,
    /// 输出是否做过敏感值脱敏。
    pub redacted: bool,
    /// 最近终端输出。
    pub output: String,
    /// 文件生成时间。
    pub generated_at: String,
}

impl AgentTerminalSnapshotContext {
    /// 校验 terminal snapshot 文件。
    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(AppError::InvalidInput(format!(
                "unsupported terminal snapshot schemaVersion: {}, expected {}",
                self.schema_version, AGENT_SESSION_SCHEMA_VERSION
            )));
        }
        AgentSessionId::new(self.agent_session_id.as_str().to_owned())?;
        Ok(())
    }
}

/// `context/workspace-snapshot.json` 内容。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentWorkspaceSnapshotContext {
    /// 文件 schema 版本。
    pub schema_version: u32,
    /// Kerminal Agent 会话主键。
    pub agent_session_id: AgentSessionId,
    /// Kerminal 全局文件优先配置目录。
    pub workspace_root: String,
    /// 当前 Agent 会话目录。
    pub session_root: String,
    /// file-backed workspace session 事实源路径。
    pub workspace_session_json: String,
    /// 文件生成时间。
    pub generated_at: String,
}

impl AgentWorkspaceSnapshotContext {
    /// 校验 workspace snapshot 文件。
    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(AppError::InvalidInput(format!(
                "unsupported workspace snapshot schemaVersion: {}, expected {}",
                self.schema_version, AGENT_SESSION_SCHEMA_VERSION
            )));
        }
        AgentSessionId::new(self.agent_session_id.as_str().to_owned())?;
        if self.workspace_root.trim().is_empty() || self.session_root.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Agent workspace snapshot paths 不能为空".to_owned(),
            ));
        }
        Ok(())
    }
}

/// `logs/mcp-calls.jsonl` 单行内容。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpCallLogEntry {
    /// 文件 schema 版本。
    pub schema_version: u32,
    /// Kerminal Agent 会话主键。
    pub agent_session_id: AgentSessionId,
    /// MCP tool id。
    pub tool_id: String,
    /// 执行状态。
    pub status: String,
    /// 简短结果摘要。
    #[serde(default)]
    pub summary: Option<String>,
    /// 错误信息。
    #[serde(default)]
    pub error: Option<String>,
    /// 脱敏运行态审计摘要。
    #[serde(default)]
    pub runtime_audit: Option<String>,
    /// 记录时间。
    pub generated_at: String,
}

impl AgentMcpCallLogEntry {
    /// 校验 MCP call log 行。
    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(AppError::InvalidInput(format!(
                "unsupported MCP call log schemaVersion: {}, expected {}",
                self.schema_version, AGENT_SESSION_SCHEMA_VERSION
            )));
        }
        AgentSessionId::new(self.agent_session_id.as_str().to_owned())?;
        if self.tool_id.trim().is_empty() || self.status.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Agent MCP call log tool_id/status 不能为空".to_owned(),
            ));
        }
        Ok(())
    }
}

/// 单条 Agent session 诊断。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionDiagnostic {
    /// 诊断文件路径。
    #[serde(default)]
    pub path: Option<String>,
    /// 诊断代码。
    pub code: String,
    /// 诊断信息。
    pub message: String,
    /// 行号。
    #[serde(default)]
    pub line: Option<usize>,
    /// 列号。
    #[serde(default)]
    pub column: Option<usize>,
}

/// list sessions 的返回值，坏文件通过 diagnostics 暴露。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionList {
    /// 成功读取的 session。
    pub sessions: Vec<AgentSessionRecord>,
    /// 被跳过或降级的坏文件诊断。
    pub diagnostics: Vec<AgentSessionDiagnostic>,
}

/// 单个 Agent session 的聚合视图。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRecord {
    /// 主 session 元数据。
    pub session: AgentSession,
    /// provider 元数据。
    #[serde(default)]
    pub provider: Option<AgentProviderSession>,
    /// 目标绑定 context。
    #[serde(default)]
    pub target_binding: Option<AgentTargetBindingContext>,
    /// MCP endpoint context。
    #[serde(default)]
    pub mcp_endpoint: Option<AgentMcpEndpointContext>,
    /// 关键文件绝对路径。
    pub paths: AgentSessionPaths,
    /// 与该 session 关联的非致命诊断。
    #[serde(default)]
    pub diagnostics: Vec<AgentSessionDiagnostic>,
}

/// 创建 Agent session 请求。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionCreateRequest {
    /// 外部 Agent 类型。
    pub agent_id: AgentId,
    /// 用户可见标题。
    #[serde(default)]
    pub title: Option<String>,
    /// 启动信息覆盖。
    #[serde(default)]
    pub launch: Option<AgentSessionLaunchRequest>,
    /// 新会话的终端操作范围；缺失时兼容旧 target/tabId 语义。
    #[serde(default)]
    pub scope: Option<AgentSessionScope>,
    /// 初始目标绑定。
    #[serde(default)]
    pub target: Option<AgentSessionTarget>,
    /// provider 元数据覆盖。
    #[serde(default)]
    pub provider: Option<AgentProviderSession>,
    /// session-scoped MCP endpoint。
    #[serde(default)]
    pub mcp_endpoint: Option<String>,
}

/// 更新 Agent session 请求。
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionUpdateRequest {
    /// 更新标题。
    #[serde(default)]
    pub title: Option<String>,
    /// 更新会话状态。
    #[serde(default)]
    pub status: Option<AgentSessionStatus>,
    /// 更新启动信息。
    #[serde(default)]
    pub launch: Option<AgentSessionLaunch>,
    /// 更新终端操作范围。
    #[serde(default)]
    pub scope: Option<AgentSessionScope>,
    /// 更新目标绑定。
    #[serde(default)]
    pub target: Option<AgentSessionTarget>,
    /// 清空目标绑定。
    #[serde(default)]
    pub clear_target: bool,
    /// 更新 provider 元数据。
    #[serde(default)]
    pub provider: Option<AgentProviderSession>,
    /// 更新 target-binding context 文件。
    #[serde(default)]
    pub target_binding_context: Option<AgentTargetBindingContext>,
    /// 更新 MCP endpoint context 文件。
    #[serde(default)]
    pub mcp_endpoint: Option<AgentMcpEndpointContext>,
}
