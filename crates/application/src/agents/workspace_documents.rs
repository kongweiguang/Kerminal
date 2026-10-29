//! Agent 工作区的受管文档生成策略；文件访问仍通过 `AgentWorkspaceRepository` 完成。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::agents::{
    AgentProvider, AgentProviderSession, AgentSession, AgentTargetLiveStatus,
};
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::{SessionInfo, SessionLifecycle};
use serde::Serialize;
use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{AppError, AppResult};

use super::{
    AgentWorkspaceDocument, AgentWorkspaceDocumentContent, AgentWorkspaceDocumentKind,
    AgentWorkspaceRepository, AgentWorkspaceRevision,
};

const SCHEMA_VERSION: u32 = 1;
const BLOCK_START: &str = "<!-- KERMINAL_EXTERNAL_AGENT_START -->";
const BLOCK_END: &str = "<!-- KERMINAL_EXTERNAL_AGENT_END -->";

/// 将聚合与原始 CAS 基线绑定；借用正文，不在组合文档时重读版本或复制整个聚合。
///
/// @author kongweiguang
pub(super) struct SessionDocumentWrite<'a> {
    pub(super) session: &'a AgentSession,
    pub(super) expected_revision: AgentWorkspaceRevision,
}

/// Provider 更新携带独立读取版本，与 session 保持在同一 journal 批次中提交。
///
/// @author kongweiguang
pub(super) struct ProviderDocumentWrite<'a> {
    pub(super) provider: &'a AgentProviderSession,
    pub(super) expected_revision: AgentWorkspaceRevision,
}

/// 快照来源、正文和预算属于同一次捕获；None 保留首选绑定规则，显式来源不触发重绑。
///
/// @author kongweiguang
pub(super) struct WorkspaceSnapshotInput<'a> {
    pub(super) target_session_id: Option<&'a SessionId>,
    pub(super) output: &'a str,
    pub(super) max_bytes: usize,
    pub(super) truncated: bool,
    pub(super) redacted: bool,
}

/// 为 Agent 新建、恢复和目标更新生成一个事务内提交的完整受管文件集。
///
/// provider 文件在写入前先读取并合并现有用户内容；context 则由运行态快照重建。
/// 快照目标可独立于首选绑定指定，记录输出时保留来源会话但不隐式改绑。
///
/// @author kongweiguang
pub(super) async fn compose_workspace_documents(
    repository: &dyn AgentWorkspaceRepository,
    session_write: SessionDocumentWrite<'_>,
    provider_update: Option<ProviderDocumentWrite<'_>>,
    endpoint: &str,
    agent_terminal: Option<&SessionInfo>,
    snapshot: WorkspaceSnapshotInput<'_>,
    cancellation: tokio_util::sync::CancellationToken,
) -> AppResult<Vec<AgentWorkspaceDocument>> {
    let SessionDocumentWrite {
        session,
        expected_revision: session_revision,
    } = session_write;
    let WorkspaceSnapshotInput {
        target_session_id: snapshot_target,
        output,
        max_bytes,
        truncated,
        redacted,
    } = snapshot;
    let target_session_id = snapshot_target
        .or_else(|| {
            session
                .target
                .as_ref()
                .and_then(|binding| binding.terminal_session_id.as_ref())
        })
        .map(SessionId::as_str);
    let mut documents = vec![serialize_session_document(session, session_revision)];
    if let Some(ProviderDocumentWrite {
        provider: provider_session,
        expected_revision,
    }) = provider_update
    {
        documents.push(serialize_provider_document(
            provider_session,
            expected_revision,
        )?);
    }
    let instructions = managed_markdown(
        repository,
        session,
        AgentWorkspaceDocumentKind::AgentInstructions,
        &agent_instructions(session, endpoint),
        cancellation.clone(),
    )
    .await?;
    documents.push(instructions);

    match session.agent_id {
        AgentProvider::Codex => {
            documents.push(codex_config(repository, session, endpoint, cancellation.clone()).await?)
        }
        AgentProvider::Claude => {
            documents.push(
                managed_markdown(
                    repository,
                    session,
                    AgentWorkspaceDocumentKind::ClaudeInstructions,
                    &claude_instructions(endpoint),
                    cancellation.clone(),
                )
                .await?,
            );
            documents.push(
                json_mcp_config(
                    repository,
                    session,
                    AgentWorkspaceDocumentKind::ClaudeMcpConfig,
                    endpoint,
                    60_000,
                    cancellation.clone(),
                )
                .await?,
            );
        }
        AgentProvider::Pi => documents.push(
            json_mcp_config(
                repository,
                session,
                AgentWorkspaceDocumentKind::PiMcpConfig,
                endpoint,
                60,
                cancellation.clone(),
            )
            .await?,
        ),
        AgentProvider::Custom => {}
    }

    documents.extend([
        json_document(
            repository,
            session,
            AgentWorkspaceDocumentKind::McpEndpointContext,
            &McpEndpointContext::new(session, Some(endpoint)),
            cancellation.clone(),
        )
        .await?,
        json_document(
            repository,
            session,
            AgentWorkspaceDocumentKind::TargetBindingContext,
            &TargetBindingContext::new(session, agent_terminal),
            cancellation.clone(),
        )
        .await?,
        json_document(
            repository,
            session,
            AgentWorkspaceDocumentKind::TerminalSnapshotContext,
            &TerminalSnapshotContext::new(
                session,
                target_session_id,
                output,
                max_bytes,
                truncated,
                redacted,
            ),
            cancellation.clone(),
        )
        .await?,
        json_document(
            repository,
            session,
            AgentWorkspaceDocumentKind::WorkspaceSnapshotContext,
            &WorkspaceSnapshotContext::new(session),
            cancellation,
        )
        .await?,
    ]);
    Ok(documents)
}

/// 按读取时 revision 生成 endpoint context，None 明确表示 listener 已停止。
pub(super) fn serialize_mcp_endpoint_document(
    session: &AgentSession,
    endpoint: Option<&str>,
    expected_revision: AgentWorkspaceRevision,
) -> AppResult<AgentWorkspaceDocument> {
    let value = McpEndpointContext::new(session, endpoint);
    let contents = serde_json::to_string_pretty(&value).map_err(|error| {
        AppError::infrastructure_with_source(
            "Agent MCP endpoint context serialization failed",
            error,
        )
    })?;
    Ok(AgentWorkspaceDocument {
        kind: AgentWorkspaceDocumentKind::McpEndpointContext,
        content: AgentWorkspaceDocumentContent::Text(contents),
        expected_revision,
    })
}

/// 将受校验的 provider 元数据放进同一批 CAS 文档，避免 session 与恢复能力分批落盘。
fn serialize_provider_document(
    provider_session: &AgentProviderSession,
    expected_revision: AgentWorkspaceRevision,
) -> AppResult<AgentWorkspaceDocument> {
    provider_session.validate().map_err(|error| {
        AppError::configuration_corrupt("Agent provider metadata").with_source(error)
    })?;
    Ok(AgentWorkspaceDocument {
        kind: AgentWorkspaceDocumentKind::Provider,
        content: AgentWorkspaceDocumentContent::Provider(provider_session.clone()),
        expected_revision,
    })
}

/// Session 文档独占聚合副本；堆布局不改变基础设施唯一适配的旧版 TOML 契约。
///
/// @author kongweiguang
pub(super) fn serialize_session_document(
    session: &AgentSession,
    expected_revision: AgentWorkspaceRevision,
) -> AgentWorkspaceDocument {
    AgentWorkspaceDocument {
        kind: AgentWorkspaceDocumentKind::Session,
        content: AgentWorkspaceDocumentContent::Session(Box::new(session.clone())),
        expected_revision,
    }
}

/// 用 application 固定的受管标记更新说明文件，并保留用户自定义段落。
async fn managed_markdown(
    repository: &dyn AgentWorkspaceRepository,
    session: &AgentSession,
    kind: AgentWorkspaceDocumentKind,
    managed: &str,
    cancellation: tokio_util::sync::CancellationToken,
) -> AppResult<AgentWorkspaceDocument> {
    let snapshot = repository
        .read_document(session.agent_session_id.clone(), kind, cancellation)
        .await?;
    let contents = patch_managed_block(snapshot.contents.as_deref().unwrap_or_default(), managed);
    Ok(AgentWorkspaceDocument {
        kind,
        content: AgentWorkspaceDocumentContent::Text(contents),
        expected_revision: snapshot.revision,
    })
}

/// 只替换成对的受管标记，缺少闭合标记时追加新块且不吞掉原正文。
fn patch_managed_block(current: &str, managed: &str) -> String {
    if let Some(start) = current.find(BLOCK_START)
        && let Some(end_relative) = current[start..].find(BLOCK_END)
    {
        let end = start + end_relative + BLOCK_END.len();
        return format!(
            "{}{}{}",
            &current[..start],
            managed.trim_end(),
            &current[end..]
        );
    }
    if current.trim().is_empty() {
        format!("{}\n", managed.trim_end())
    } else {
        format!("{}\n\n{}\n", current.trim_end(), managed.trim_end())
    }
}

/// 更新 provider 原生 MCP JSON 但保留其它 server、未知配置字段和用户自定义值。
async fn json_mcp_config(
    repository: &dyn AgentWorkspaceRepository,
    session: &AgentSession,
    kind: AgentWorkspaceDocumentKind,
    endpoint: &str,
    timeout: u64,
    cancellation: tokio_util::sync::CancellationToken,
) -> AppResult<AgentWorkspaceDocument> {
    let snapshot = repository
        .read_document(session.agent_session_id.clone(), kind, cancellation)
        .await?;
    let mut root = match snapshot.contents.as_deref() {
        Some(source) if !source.trim().is_empty() => serde_json::from_str::<Value>(source)
            .map_err(|error| {
                AppError::configuration_corrupt("Agent MCP JSON").with_source(error)
            })?,
        _ => json!({}),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| AppError::configuration_corrupt("Agent MCP JSON"))?;
    let servers = ensure_json_object(object, "mcpServers")?;
    let server = ensure_json_object(servers, "kerminal")?;
    server.insert("type".to_owned(), Value::String("http".to_owned()));
    server.insert("url".to_owned(), Value::String(endpoint.to_owned()));
    server.insert("timeout".to_owned(), json!(timeout));
    let contents = serde_json::to_string_pretty(&root).map_err(|error| {
        AppError::infrastructure_with_source("Agent MCP JSON serialization failed", error)
    })?;
    Ok(AgentWorkspaceDocument {
        kind,
        content: AgentWorkspaceDocumentContent::Text(contents),
        expected_revision: snapshot.revision,
    })
}

/// 为 Codex 设置 Kerminal MCP endpoint，同时用 TOML 编辑器保留注释及其他配置。
async fn codex_config(
    repository: &dyn AgentWorkspaceRepository,
    session: &AgentSession,
    endpoint: &str,
    cancellation: tokio_util::sync::CancellationToken,
) -> AppResult<AgentWorkspaceDocument> {
    let kind = AgentWorkspaceDocumentKind::CodexConfig;
    let snapshot = repository
        .read_document(session.agent_session_id.clone(), kind, cancellation)
        .await?;
    let mut document = match snapshot.contents.as_deref() {
        Some(source) if !source.trim().is_empty() => {
            source.parse::<DocumentMut>().map_err(|error| {
                AppError::configuration_corrupt("Codex config.toml").with_source(error)
            })?
        }
        _ => DocumentMut::new(),
    };
    let servers = ensure_toml_table(document.as_table_mut(), "mcp_servers")?;
    let server = ensure_toml_table(servers, "kerminal")?;
    server.insert("url", value(endpoint));
    server.insert("default_tools_approval_mode", value("prompt"));
    server.insert("tool_timeout_sec", value(60));
    server.insert("enabled", value(true));
    Ok(AgentWorkspaceDocument {
        kind,
        content: AgentWorkspaceDocumentContent::Text(document.to_string()),
        expected_revision: snapshot.revision,
    })
}

/// 确保 MCP JSON 的既有层级仍是 object；错误类型结构不能被默认值覆盖。
fn ensure_json_object<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> AppResult<&'a mut Map<String, Value>> {
    parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| AppError::configuration_corrupt("Agent MCP JSON"))
}

/// 确保 Codex MCP 配置层级为 TOML table，不通过文本替换破坏注释和相邻字段。
fn ensure_toml_table<'a>(parent: &'a mut Table, key: &str) -> AppResult<&'a mut Table> {
    parent
        .entry(key)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| AppError::configuration_corrupt("Codex config.toml"))
}

/// 所有 provider 使用唯一 live 上下文入口与保留的运行态工具；审批仍由 host 决定。
fn agent_instructions(session: &AgentSession, endpoint: &str) -> String {
    let id = session.agent_session_id.as_str();
    format!(
        r#"{BLOCK_START}
# Kerminal Agent session

- This session has global Kerminal runtime access. Its preferred terminal is context only, not an authorization boundary.
- Before using Kerminal, read `session.toml` and `context/mcp-endpoint.json`, `context/target-binding.json`, `context/terminal-snapshot.json`, and `context/workspace-snapshot.json`.
- Refresh live state with `kerminal.agent.target_context`, passing `agentSessionId = "{id}"`.
- Use `kerminal.help` for tool discovery; `topic = "config"` returns configuration rules.
- The session endpoint is `{endpoint}`. Use its MCP tools directly; the MCP host owns approval and audit policy.
- External MCP defaults to background execution. A visible Tab is never required. When a preferred target is stale or missing, report that state and do not substitute another terminal.
- Run non-interactive server commands with `ssh.command`. For local/remote file transfers use `sftp.transfer.enqueue`, then query `sftp.transfer.list` until completion. Container and tmux commands use `ssh.command`; their former dedicated MCP tools are unavailable.
- For a new interactive shell, call `terminal.create`, then use its explicit `sessionId` for read/write and close it after the task. For target-bound work, refresh `kerminal.agent.target_context` first; ordinary Tab changes do not rebind this session.
- Keep custom text outside the managed block below; Kerminal updates only this marked block.
{BLOCK_END}
"#
    )
}

/// Claude 复用同一目标上下文规则，避免 provider 文档保留已移除工具造成循环重试。
fn claude_instructions(endpoint: &str) -> String {
    format!(
        r#"{BLOCK_START}
@AGENTS.md

## Claude Code

- Follow `AGENTS.md` and refresh the live target with `kerminal.agent.target_context` before using a preferred terminal.
- MCP host policy owns confirmation, approval, permissions, hooks, and audit. Kerminal adds no second prompt.
- Use the session scoped Kerminal endpoint `{endpoint}`. A stale preferred target never falls back to another Tab.
{BLOCK_END}
"#
    )
}

/// 序列化一个固定路径的 JSON context 文件，避免调用者传入路径或暴露绝对目录。
async fn json_document<T: Serialize>(
    repository: &dyn AgentWorkspaceRepository,
    session: &AgentSession,
    kind: AgentWorkspaceDocumentKind,
    value: &T,
    cancellation: tokio_util::sync::CancellationToken,
) -> AppResult<AgentWorkspaceDocument> {
    let contents = serde_json::to_string_pretty(value).map_err(|error| {
        AppError::infrastructure_with_source("Agent context serialization failed", error)
    })?;
    let snapshot = repository
        .read_document(session.agent_session_id.clone(), kind, cancellation)
        .await?;
    Ok(AgentWorkspaceDocument {
        kind,
        content: AgentWorkspaceDocumentContent::Text(contents),
        expected_revision: snapshot.revision,
    })
}

/// 生成 UTC epoch 时间戳字符串，兼容现有 Agent session schema 的秒级格式。
fn current_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpEndpointContext<'a> {
    schema_version: u32,
    agent_session_id: &'a str,
    endpoint: Option<&'a str>,
    transport: &'static str,
    tools_only: bool,
    generated_at: String,
}

impl<'a> McpEndpointContext<'a> {
    /// 固定 Streamable HTTP context schema，供 Agent 初始化时发现自身 MCP endpoint。
    fn new(session: &'a AgentSession, endpoint: Option<&'a str>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            agent_session_id: session.agent_session_id.as_str(),
            endpoint,
            transport: "streamable-http",
            tools_only: true,
            generated_at: current_timestamp(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetBindingContext<'a> {
    schema_version: u32,
    agent_session_id: &'a str,
    scope: AgentScopeContext,
    binding: TargetBindingSnapshot<'a>,
    agent_terminal: AgentTerminalSnapshot,
    generated_at: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum AgentScopeContext {
    Global,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetBindingSnapshot<'a> {
    binding_id: Option<String>,
    generation: u64,
    status: &'static str,
    stale: bool,
    pane_id: Option<String>,
    tab_id: Option<String>,
    target_terminal_session_id: Option<String>,
    target_ref: Option<&'a str>,
    cwd: Option<&'a str>,
    shell: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTerminalSnapshot {
    session_id: Option<String>,
    status: &'static str,
}

impl<'a> TargetBindingContext<'a> {
    /// 仅持久化显式首选目标状态；Agent 自己的 CLI 不会进入用户目标列表。
    fn new(session: &'a AgentSession, terminal: Option<&'a SessionInfo>) -> Self {
        let binding = match session.target.as_ref() {
            Some(target) => TargetBindingSnapshot {
                binding_id: target.binding_id.clone(),
                generation: target.generation,
                status: target_status(target.live_status),
                stale: matches!(
                    target.live_status,
                    AgentTargetLiveStatus::Stale | AgentTargetLiveStatus::Closed
                ),
                pane_id: target.pane_id.as_ref().map(ToString::to_string),
                tab_id: target.tab_id.as_ref().map(ToString::to_string),
                target_terminal_session_id: target
                    .terminal_session_id
                    .as_ref()
                    .map(ToString::to_string),
                target_ref: target.target_ref.as_deref(),
                cwd: target.cwd.as_deref(),
                shell: target.shell.as_deref(),
            },
            None => TargetBindingSnapshot {
                binding_id: None,
                generation: 0,
                status: "unbound",
                stale: false,
                pane_id: None,
                tab_id: None,
                target_terminal_session_id: None,
                target_ref: None,
                cwd: None,
                shell: None,
            },
        };
        Self {
            schema_version: SCHEMA_VERSION,
            agent_session_id: session.agent_session_id.as_str(),
            scope: AgentScopeContext::Global,
            binding,
            agent_terminal: AgentTerminalSnapshot {
                session_id: terminal.map(|info| info.session_id.to_string()),
                status: terminal
                    .map(|info| lifecycle_status(info.lifecycle))
                    .unwrap_or("stopped"),
            },
            generated_at: current_timestamp(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalSnapshotContext<'a> {
    schema_version: u32,
    agent_session_id: &'a str,
    target_terminal_session_id: Option<&'a str>,
    captured_bytes: usize,
    max_bytes: usize,
    truncated: bool,
    redacted: bool,
    output: &'a str,
    generated_at: String,
}

impl<'a> TerminalSnapshotContext<'a> {
    /// 保存实际有界并脱敏的输出，携带截断和脱敏元数据供 Agent 正确理解。
    fn new(
        session: &'a AgentSession,
        target_session_id: Option<&'a str>,
        output: &'a str,
        max_bytes: usize,
        truncated: bool,
        redacted: bool,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            agent_session_id: session.agent_session_id.as_str(),
            target_terminal_session_id: target_session_id,
            captured_bytes: output.len(),
            max_bytes,
            truncated,
            redacted,
            output,
            generated_at: current_timestamp(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceSnapshotContext<'a> {
    schema_version: u32,
    agent_session_id: &'a str,
    workspace_root: &'a str,
    session_root: &'a str,
    workspace_session_json: String,
    generated_at: String,
}

impl<'a> WorkspaceSnapshotContext<'a> {
    /// 记录 workspace snapshot 固定路径，避免 Agent 根据绝对目录猜测持久状态文件。
    fn new(session: &'a AgentSession) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            agent_session_id: session.agent_session_id.as_str(),
            workspace_root: &session.workspace_root,
            session_root: &session.session_root,
            workspace_session_json: PathBuf::from(&session.workspace_root)
                .join("workspace")
                .join("session.json")
                .to_string_lossy()
                .into_owned(),
            generated_at: current_timestamp(),
        }
    }
}

/// 把领域状态编码成 context 文件兼容的小写枚举。
fn target_status(status: AgentTargetLiveStatus) -> &'static str {
    match status {
        AgentTargetLiveStatus::Unbound => "unbound",
        AgentTargetLiveStatus::Ready => "ready",
        AgentTargetLiveStatus::Stale => "stale",
        AgentTargetLiveStatus::Closed => "closed",
    }
}

/// 把终端实际 lifecycle 投影到 Agent context 使用的稳定小写状态。
fn lifecycle_status(status: SessionLifecycle) -> &'static str {
    match status {
        SessionLifecycle::Starting => "starting",
        SessionLifecycle::Running => "running",
        SessionLifecycle::Closing => "closing",
        SessionLifecycle::Exited => "exited",
        SessionLifecycle::Closed => "closed",
        SessionLifecycle::Failed => "failed",
    }
}

#[cfg(test)]
#[path = "../../tests/unit/agent_workspace_documents.rs"]
mod tests;
