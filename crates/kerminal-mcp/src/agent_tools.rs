//! Agent 会话运行态工具的协议映射。
//!
//! @author kongweiguang

use kerminal_application::{AppError, sessions::SessionFilter};
use kerminal_domain::identity::AgentSessionId;
use kerminal_domain::sessions::{SessionLifecycle, SessionOwner};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{McpServices, terminal_tools};

const DEFAULT_CONTEXT_BYTES: usize = 24 * 1024;
const MAX_CONTEXT_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentSessionArgs {
    agent_session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentContextArgs {
    agent_session_id: String,
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// 查询运行中的 Agent session，并返回与旧 IPC 一致的完整安全记录投影。
pub(crate) async fn current_session(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return terminal_tools::failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match terminal_tools::parse_args::<AgentSessionArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return terminal_tools::failure(error),
    };
    let agent_id = match resolve_agent_id(services, Some(&args.agent_session_id)) {
        Ok(id) => id,
        Err(error) => return terminal_tools::failure(error),
    };
    if services.agent_scope.is_some()
        && let Err(error) = terminal_tools::ensure_live_agent_scope(
            services,
            agent_id.clone(),
            cancellation.clone(),
        )
        .await
    {
        return terminal_tools::failure(error);
    }
    let service = match services.agent_service() {
        Ok(service) => service,
        Err(error) => return terminal_tools::failure(error),
    };
    match service.record_projection(agent_id, cancellation).await {
        Ok(record) => {
            if let Some(recovery) = record.resume_recovery.as_ref() {
                return terminal_tools::failure_with_data(
                    AppError::outcome_unknown("Agent resume metadata needs readback"),
                    super::agent_wire::resume_recovery_data(recovery),
                );
            }
            let entity_id = record.session.agent_session_id.clone();
            match super::agent_wire::session_record_value(&record) {
                Ok(record) => terminal_tools::success(
                    "已读取 Kerminal Agent 会话。".to_owned(),
                    json!({ "agentSession": record }),
                    vec![json!({ "type": "agentSession", "id": entity_id })],
                ),
                Err(error) => terminal_tools::failure(error),
            }
        }
        Err(error) => terminal_tools::failure(error),
    }
}

/// 解析 Agent 首选终端的显式 session id 与 live/stale 状态，不检索活动 Tab。
pub(crate) async fn resolve_target(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return terminal_tools::failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match terminal_tools::parse_args::<AgentSessionArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return terminal_tools::failure(error),
    };
    let agent_id = match resolve_agent_id(services, Some(&args.agent_session_id)) {
        Ok(id) => id,
        Err(error) => return terminal_tools::failure(error),
    };
    if services.agent_scope.is_some()
        && let Err(error) = terminal_tools::ensure_live_agent_scope(
            services,
            agent_id.clone(),
            cancellation.clone(),
        )
        .await
    {
        return terminal_tools::failure(error);
    }
    let service = match services.agent_service() {
        Ok(service) => service,
        Err(error) => return terminal_tools::failure(error),
    };
    match service
        .resolve_preferred_target(agent_id, cancellation)
        .await
    {
        Ok(resolution) => {
            let binding = match super::agent_wire::target_binding_value(&resolution) {
                Ok(binding) => binding,
                Err(error) => return terminal_tools::failure(error),
            };
            let entity = match super::agent_wire::target_binding_entity_value(&resolution) {
                Ok(entity) => entity,
                Err(error) => return terminal_tools::failure(error),
            };
            terminal_tools::success(
                "已解析 Agent 首选终端绑定。".to_owned(),
                json!({ "targetBinding": binding }),
                vec![entity],
            )
        }
        Err(error) => terminal_tools::failure(error),
    }
}

/// 读取 Agent 会话及其绑定目标的有界终端上下文快照。
pub(crate) async fn target_context(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return terminal_tools::failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match terminal_tools::parse_args::<AgentContextArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return terminal_tools::failure(error),
    };
    let max_bytes = args.max_bytes.unwrap_or(DEFAULT_CONTEXT_BYTES);
    if max_bytes == 0 || max_bytes > MAX_CONTEXT_BYTES {
        return terminal_tools::failure(AppError::invalid(
            "maxBytes must be between 1 and 1048576",
        ));
    }
    let agent_id = match resolve_agent_id(services, Some(&args.agent_session_id)) {
        Ok(id) => id,
        Err(error) => return terminal_tools::failure(error),
    };
    if services.agent_scope.is_some()
        && let Err(error) = terminal_tools::ensure_live_agent_scope(
            services,
            agent_id.clone(),
            cancellation.clone(),
        )
        .await
    {
        return terminal_tools::failure(error);
    }
    let service = match services.agent_service() {
        Ok(service) => service,
        Err(error) => return terminal_tools::failure(error),
    };
    match service
        .target_context(agent_id, max_bytes, cancellation.clone())
        .await
    {
        Ok(context) => {
            if let Some(recovery) = context
                .record
                .resume_recovery
                .as_ref()
                .or(context.session.resume_recovery.as_ref())
            {
                return terminal_tools::failure_with_data(
                    AppError::outcome_unknown("Agent resume metadata needs readback"),
                    super::agent_wire::resume_recovery_data(recovery),
                );
            }
            let sessions = match services.sessions.list(SessionFilter::default()).await {
                Ok(sessions) => sessions,
                Err(error) => return terminal_tools::failure(error),
            };
            let mut summaries = Vec::new();
            for session in sessions.iter().filter(|session| {
                session.lifecycle == SessionLifecycle::Running
                    && !matches!(session.owner, SessionOwner::Agent { .. })
            }) {
                if cancellation.is_cancelled() {
                    return terminal_tools::failure(AppError::cancelled(
                        "MCP request was cancelled",
                    ));
                }
                let mut summary = terminal_tools::session_summary(services, session).await;
                if let SessionOwner::Ui { tab_id, pane_id } = &session.owner
                    && let Some(object) = summary.as_object_mut()
                {
                    object.insert("tabId".to_owned(), json!(tab_id.as_str()));
                    object.insert("paneId".to_owned(), json!(pane_id.as_str()));
                }
                summaries.push((session.session_id.clone(), summary));
            }

            let terminal_summary = match (&context.snapshot, &context.target.resolved_session_id) {
                (Some(_), Some(target_id)) => match summaries
                    .iter()
                    .find(|(session_id, _)| session_id == target_id)
                {
                    Some((_, summary)) => Some(summary.clone()),
                    None => {
                        return terminal_tools::failure(AppError::conflict(
                            "Agent target changed while its context was being read",
                        ));
                    }
                },
                (Some(_), None) => {
                    return terminal_tools::failure(AppError::conflict(
                        "Agent snapshot has no resolved target session",
                    ));
                }
                (None, _) => None,
            };
            let terminals = summaries.into_iter().map(|(_, summary)| summary).collect();
            match super::agent_wire::target_context_value(
                &context,
                terminals,
                terminal_summary,
                max_bytes,
            ) {
                Ok(value) => terminal_tools::success(
                    "已读取 Agent 目标上下文。".to_owned(),
                    value,
                    Vec::new(),
                ),
                Err(error) => terminal_tools::failure(error),
            }
        }
        Err(error) => terminal_tools::failure(error),
    }
}

/// 将 route scope 与显式 id 合并；session-scoped endpoint 不允许 id 歧义。
fn resolve_agent_id(
    services: &McpServices,
    explicit_id: Option<&str>,
) -> Result<AgentSessionId, AppError> {
    let explicit = explicit_id
        .map(AgentSessionId::parse)
        .transpose()
        .map_err(|_| AppError::invalid("agent session id"))?;
    match (&services.agent_scope, explicit) {
        (Some(scoped), Some(explicit)) if *scoped != explicit => Err(AppError::conflict(
            "Agent endpoint session does not match agentSessionId",
        )),
        (Some(scoped), _) => Ok(scoped.clone()),
        (None, Some(explicit)) => Ok(explicit),
        (None, None) => Err(AppError::invalid("agentSessionId is required")),
    }
}
