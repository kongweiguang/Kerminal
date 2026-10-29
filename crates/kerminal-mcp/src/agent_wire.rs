//! Agent MCP 响应的显式 wire 投影，隔离领域枚举和内部字段命名。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError,
    agents::{
        AgentMcpEndpointContextProjection, AgentResumeRecovery, AgentResumeRecoveryStatus,
        AgentSessionContextPathsProjection, AgentSessionPathsProjection,
        AgentSessionRecordDiagnostic, AgentSessionRecordProjection,
        AgentSessionRecordSessionProjection, AgentTargetBindingContextBindingProjection,
        AgentTargetBindingContextProjection, AgentTargetContext, AgentTargetResolution,
    },
};
use kerminal_domain::agents::{
    AgentProviderSession, AgentSessionScope, AgentSessionStatus, AgentTargetBinding,
    AgentTargetLiveStatus,
};
use serde::Serialize;
use serde_json::Value;

/// 将未确认的恢复操作映射为错误 data 中的可追踪原 session 标识。
pub(crate) fn resume_recovery_data(recovery: &AgentResumeRecovery) -> Value {
    let status = match recovery.status {
        AgentResumeRecoveryStatus::Pending => "pending",
        AgentResumeRecoveryStatus::Unknown => "unknown",
    };
    serde_json::json!({
        "terminalSessionId": recovery.terminal_session_id.as_str(),
        "status": status
    })
}

/// 将 Agent 上下文投影成旧版顶层结构，终端摘要由统一 session summary mapper 提供。
pub(crate) fn target_context_value(
    context: &AgentTargetContext,
    terminals: Vec<Value>,
    terminal_summary: Option<Value>,
    max_bytes: usize,
) -> Result<Value, AppError> {
    let terminal = match (&context.snapshot, terminal_summary) {
        (Some(snapshot), Some(session)) => Some(AgentTargetTerminalWire {
            session,
            snapshot: AgentTargetSnapshotTextWire {
                data: snapshot.text.clone(),
                captured_bytes: context.captured_bytes,
                max_bytes,
                truncated: snapshot.truncated,
                redacted: context.redacted,
            },
        }),
        (None, None) => None,
        (Some(_), None) => {
            return Err(AppError::conflict(
                "Agent target changed while its context was being read",
            ));
        }
        (None, Some(_)) => {
            return Err(AppError::infrastructure(
                "Agent target summary exists without a terminal snapshot",
            ));
        }
    };
    let target_binding = match context.target.status {
        AgentTargetLiveStatus::Unbound => None,
        _ => Some(AgentTargetBindingSnapshotWire::from_resolution(
            &context.target,
        )?),
    };
    to_value(AgentTargetContextWire {
        agent_session: AgentSessionRecordWire::from_projection(&context.record)?,
        scope: AgentSessionScopeWire::from_domain(&context.session.session.effective_scope())?,
        target_binding,
        terminals,
        terminal,
    })
}

/// 显式生成旧版 AgentSessionRecord，避免外部格式依赖 serde 对领域对象的推导。
impl AgentSessionRecordWire {
    /// 逐层转换记录，确保每个领域值都经过对应 wire 映射。
    fn from_projection(projection: &AgentSessionRecordProjection) -> Result<Self, AppError> {
        Ok(Self {
            session: AgentSessionRecordSessionWire::from_projection(&projection.session)?,
            provider: projection
                .provider
                .as_ref()
                .map(AgentProviderSessionWire::from_domain),
            target_binding: projection
                .target_binding
                .as_ref()
                .map(AgentTargetBindingContextWire::from_projection)
                .transpose()?,
            mcp_endpoint: projection
                .mcp_endpoint
                .as_ref()
                .map(AgentMcpEndpointContextWire::from_projection),
            paths: AgentSessionPathsWire::from_projection(&projection.paths),
            diagnostics: projection
                .diagnostics
                .iter()
                .map(AgentSessionDiagnosticWire::from_projection)
                .collect(),
        })
    }
}

/// 将已校验的 application DTO 编码为 JSON，序列化失败保留内部 source 便于诊断。
fn to_value<T: Serialize>(wire: T) -> Result<Value, AppError> {
    serde_json::to_value(wire).map_err(|error| {
        AppError::infrastructure_with_source("Agent MCP response serialization failed", error)
    })
}

/// 外层记录沿用 AgentSessionRecord 的 camelCase；内层 session 保留模型原生 snake_case。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionRecordWire {
    session: AgentSessionRecordSessionWire,
    provider: Option<AgentProviderSessionWire>,
    target_binding: Option<AgentTargetBindingContextWire>,
    mcp_endpoint: Option<AgentMcpEndpointContextWire>,
    paths: AgentSessionPathsWire,
    diagnostics: Vec<AgentSessionDiagnosticWire>,
}

/// 旧 MCP 将 AgentSession 直接序列化，故这里的字段名按模型 serde 的 snake_case 固定。
#[derive(Serialize)]
struct AgentSessionRecordSessionWire {
    schema_version: u32,
    agent_session_id: String,
    agent_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    launcher_key: Option<String>,
    title: String,
    created_at: String,
    updated_at: String,
    status: &'static str,
    workspace_root: String,
    session_root: String,
    launch: AgentSessionLaunchWire,
    scope: Option<AgentSessionScopeWire>,
    target: Option<AgentSessionTargetWire>,
}

impl AgentSessionRecordSessionWire {
    /// 映射旧 session.toml 字段并有意不暴露启动环境值。
    fn from_projection(projection: &AgentSessionRecordSessionProjection) -> Result<Self, AppError> {
        Ok(Self {
            schema_version: projection.schema_version,
            agent_session_id: projection.agent_session_id.clone(),
            agent_id: projection.agent_id.as_str(),
            launcher_key: projection.launcher_key.clone(),
            title: projection.title.clone(),
            created_at: projection.created_at.clone(),
            updated_at: projection.updated_at.clone(),
            status: session_status(projection.status),
            workspace_root: projection.workspace_root.clone(),
            session_root: projection.session_root.clone(),
            launch: AgentSessionLaunchWire {
                command_label: projection.launch.command_label.clone(),
                shell: projection.launch.shell.clone(),
                args: projection.launch.args.clone(),
                cwd: projection.launch.cwd.clone(),
            },
            scope: projection
                .scope
                .as_ref()
                .map(AgentSessionScopeWire::from_domain)
                .transpose()?,
            target: projection
                .target
                .as_ref()
                .map(AgentSessionTargetWire::from_domain),
        })
    }
}

#[derive(Serialize)]
struct AgentSessionLaunchWire {
    command_label: String,
    shell: String,
    args: Vec<String>,
    cwd: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum AgentSessionScopeWire {
    Tab { tab_id: String },
    Global,
}

impl AgentSessionScopeWire {
    /// 领域 scope 只允许通过显式分支进入 MCP wire。
    fn from_domain(scope: &AgentSessionScope) -> Result<Self, AppError> {
        match scope {
            AgentSessionScope::Tab { .. } => scope
                .tab_id()
                .map(|tab_id| Self::Tab {
                    tab_id: tab_id.as_str().to_owned(),
                })
                .ok_or_else(|| AppError::infrastructure("Tab scope is missing its identity")),
            AgentSessionScope::Global => Ok(Self::Global),
        }
    }
}

#[derive(Serialize)]
struct AgentSessionTargetWire {
    binding_id: Option<String>,
    binding_generation: u64,
    pane_id: Option<String>,
    tab_id: Option<String>,
    target_terminal_session_id: Option<String>,
    target_ref: Option<String>,
    target_kind: Option<String>,
    cwd: Option<String>,
    shell: Option<String>,
    live_status: &'static str,
    last_seen_at: Option<String>,
}

impl AgentSessionTargetWire {
    /// 将首选目标值对象映射到旧 snake_case session.target 字段与小写状态。
    fn from_domain(target: &AgentTargetBinding) -> Self {
        Self {
            binding_id: target.binding_id.clone(),
            binding_generation: target.generation,
            pane_id: target.pane_id.as_ref().map(ToString::to_string),
            tab_id: target.tab_id.as_ref().map(ToString::to_string),
            target_terminal_session_id: target
                .terminal_session_id
                .as_ref()
                .map(ToString::to_string),
            target_ref: target.target_ref.clone(),
            target_kind: target.target_kind.clone(),
            cwd: target.cwd.clone(),
            shell: target.shell.clone(),
            live_status: target_live_status(target.live_status),
            last_seen_at: target.last_seen_at.clone(),
        }
    }
}

#[derive(Serialize)]
struct AgentProviderSessionWire {
    schema_version: u32,
    provider: &'static str,
    provider_session_id: Option<String>,
    resume_command: Option<String>,
    resume_supported: bool,
    last_resume_at: Option<String>,
}

impl AgentProviderSessionWire {
    /// 按旧 ProviderSession serde 的 snake_case 投影已验证字段，不暴露值对象自身 serde。
    fn from_domain(provider: &AgentProviderSession) -> Self {
        Self {
            schema_version: provider.schema_version,
            provider: provider.provider.as_str(),
            provider_session_id: provider.provider_session_id.clone(),
            resume_command: provider.resume_command.clone(),
            resume_supported: provider.resume_supported,
            last_resume_at: provider.last_resume_at.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTargetBindingContextWire {
    schema_version: u32,
    agent_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<AgentSessionScopeWire>,
    binding: AgentTargetBindingContextBindingWire,
    agent_terminal: Option<AgentTerminalContextWire>,
    generated_at: String,
}

impl AgentTargetBindingContextWire {
    /// 保留固定 target-binding.json 的旧嵌套结构，状态值不经领域 serde。
    fn from_projection(projection: &AgentTargetBindingContextProjection) -> Result<Self, AppError> {
        Ok(Self {
            schema_version: projection.schema_version,
            agent_session_id: projection.agent_session_id.clone(),
            scope: projection
                .scope
                .as_ref()
                .map(AgentSessionScopeWire::from_domain)
                .transpose()?,
            binding: AgentTargetBindingContextBindingWire::from_projection(&projection.binding),
            agent_terminal: projection
                .agent_terminal
                .as_ref()
                .map(AgentTerminalContextWire::from_projection),
            generated_at: projection.generated_at.clone(),
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTargetBindingContextBindingWire {
    binding_id: Option<String>,
    generation: u64,
    status: String,
    stale: bool,
    pane_id: Option<String>,
    tab_id: Option<String>,
    target_terminal_session_id: Option<String>,
    target_ref: Option<String>,
    cwd: Option<String>,
    shell: Option<String>,
}

impl AgentTargetBindingContextBindingWire {
    /// 应用层只接受已知状态后才创建该投影，避免透传损坏文档中的任意枚举值。
    fn from_projection(projection: &AgentTargetBindingContextBindingProjection) -> Self {
        Self {
            binding_id: projection.binding_id.clone(),
            generation: projection.generation,
            status: projection.status.clone(),
            stale: projection.stale,
            pane_id: projection.pane_id.clone(),
            tab_id: projection.tab_id.clone(),
            target_terminal_session_id: projection.target_terminal_session_id.clone(),
            target_ref: projection.target_ref.clone(),
            cwd: projection.cwd.clone(),
            shell: projection.shell.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTerminalContextWire {
    session_id: Option<String>,
    status: String,
}

impl AgentTerminalContextWire {
    /// 暴露 context 文档中已保存的 Agent CLI 状态，不从 target 终端推断。
    fn from_projection(
        projection: &kerminal_application::agents::AgentTerminalContextProjection,
    ) -> Self {
        Self {
            session_id: projection.session_id.clone(),
            status: projection.status.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentMcpEndpointContextWire {
    schema_version: u32,
    agent_session_id: String,
    endpoint: Option<String>,
    transport: String,
    tools_only: bool,
    generated_at: String,
}

impl AgentMcpEndpointContextWire {
    /// 映射 workspace endpoint 文档；缺失 endpoint 继续以 JSON null 表示。
    fn from_projection(projection: &AgentMcpEndpointContextProjection) -> Self {
        Self {
            schema_version: projection.schema_version,
            agent_session_id: projection.agent_session_id.clone(),
            endpoint: projection.endpoint.clone(),
            transport: projection.transport.clone(),
            tools_only: projection.tools_only,
            generated_at: projection.generated_at.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionPathsWire {
    workspace_root: String,
    session_root: String,
    session_toml: String,
    provider_toml: String,
    context: AgentSessionContextPathsWire,
}

impl AgentSessionPathsWire {
    /// 映射 session 路径和三份受管 context 文件，路径来自 workspace repository。
    fn from_projection(projection: &AgentSessionPathsProjection) -> Self {
        Self {
            workspace_root: projection.workspace_root.clone(),
            session_root: projection.session_root.clone(),
            session_toml: projection.session_toml.clone(),
            provider_toml: projection.provider_toml.clone(),
            context: AgentSessionContextPathsWire::from_projection(&projection.context),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionContextPathsWire {
    target_binding_json: String,
    mcp_endpoint_json: String,
    terminal_snapshot_json: String,
    workspace_snapshot_json: String,
}

impl AgentSessionContextPathsWire {
    /// 保留旧 paths.context 文件字段，方便调用方按规则直接读取真实受管路径。
    fn from_projection(projection: &AgentSessionContextPathsProjection) -> Self {
        Self {
            target_binding_json: projection.target_binding_json.clone(),
            mcp_endpoint_json: projection.mcp_endpoint_json.clone(),
            terminal_snapshot_json: projection.terminal_snapshot_json.clone(),
            workspace_snapshot_json: projection.workspace_snapshot_json.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionDiagnosticWire {
    path: Option<String>,
    code: String,
    message: String,
    line: Option<usize>,
    column: Option<usize>,
}

impl AgentSessionDiagnosticWire {
    /// 把稳定且已脱敏的 application 诊断映射为旧版诊断字段。
    fn from_projection(projection: &AgentSessionRecordDiagnostic) -> Self {
        Self {
            path: projection.path.clone(),
            code: projection.code.clone(),
            message: projection.message.clone(),
            line: projection.line,
            column: projection.column,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTargetBindingSnapshotWire {
    agent_session_id: String,
    target_terminal_session_id: String,
    pane_id: String,
    tab_id: Option<String>,
    target_ref: Option<String>,
    cwd: Option<String>,
    shell: Option<String>,
    binding_id: String,
    generation: u64,
    status: AgentTargetBindingSnapshotStatusWire,
    live: bool,
    stale: bool,
    updated_at_ms: u64,
}

impl AgentTargetBindingSnapshotWire {
    /// 仅构造旧 snapshot 必需且来自真实绑定的数据，不合成缺失 id 或 pane。
    fn from_resolution(resolution: &AgentTargetResolution) -> Result<Self, AppError> {
        let binding = &resolution.binding;
        let target_terminal_session_id = binding
            .terminal_session_id
            .as_ref()
            .ok_or_else(|| AppError::not_found("Agent target binding not found"))?;
        let pane_id = binding
            .pane_id
            .as_ref()
            .ok_or_else(|| AppError::not_found("Agent target binding pane not found"))?;
        let binding_id = binding
            .binding_id
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AppError::conflict("Agent target binding has no stable binding id"))?;
        let status = AgentTargetBindingSnapshotStatusWire::from_domain(resolution.status)?;
        Ok(Self {
            agent_session_id: resolution.agent_session_id.as_str().to_owned(),
            target_terminal_session_id: target_terminal_session_id.as_str().to_owned(),
            pane_id: pane_id.as_str().to_owned(),
            tab_id: binding
                .tab_id
                .as_ref()
                .map(|tab_id| tab_id.as_str().to_owned()),
            target_ref: binding.target_ref.clone(),
            cwd: binding.cwd.clone(),
            shell: binding.shell.clone(),
            binding_id: binding_id.clone(),
            generation: binding.generation,
            status,
            live: resolution.status == AgentTargetLiveStatus::Ready,
            stale: resolution.status == AgentTargetLiveStatus::Stale,
            updated_at_ms: resolution.observed_at_ms,
        })
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum AgentTargetBindingSnapshotStatusWire {
    Live,
    Stale,
    Closed,
}

impl AgentTargetBindingSnapshotStatusWire {
    /// 旧 targetBinding snapshot 没有 Unbound 状态；缺绑定由调用方按原错误表示。
    fn from_domain(status: AgentTargetLiveStatus) -> Result<Self, AppError> {
        match status {
            AgentTargetLiveStatus::Ready => Ok(Self::Live),
            AgentTargetLiveStatus::Stale => Ok(Self::Stale),
            AgentTargetLiveStatus::Closed => Ok(Self::Closed),
            AgentTargetLiveStatus::Unbound => {
                Err(AppError::not_found("Agent target binding not found"))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTargetContextWire {
    agent_session: AgentSessionRecordWire,
    scope: AgentSessionScopeWire,
    target_binding: Option<AgentTargetBindingSnapshotWire>,
    terminals: Vec<Value>,
    terminal: Option<AgentTargetTerminalWire>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTargetTerminalWire {
    session: Value,
    snapshot: AgentTargetSnapshotTextWire,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTargetSnapshotTextWire {
    data: String,
    captured_bytes: usize,
    max_bytes: usize,
    truncated: bool,
    redacted: bool,
}

/// 将生命周期 enum 映射为 v0.3.45 的小写 session status。
fn session_status(status: AgentSessionStatus) -> &'static str {
    match status {
        AgentSessionStatus::Active => "active",
        AgentSessionStatus::Archived => "archived",
        AgentSessionStatus::Stale => "stale",
    }
}

/// 将绑定状态映射为 session.target 使用的小写 liveStatus。
fn target_live_status(status: AgentTargetLiveStatus) -> &'static str {
    match status {
        AgentTargetLiveStatus::Unbound => "unbound",
        AgentTargetLiveStatus::Ready => "ready",
        AgentTargetLiveStatus::Stale => "stale",
        AgentTargetLiveStatus::Closed => "closed",
    }
}

#[cfg(test)]
#[path = "../tests/unit/agent_wire_contract.rs"]
mod contract_tests;
