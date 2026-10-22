//! MCP 终端工具到 SessionService 的显式适配。
//!
//! @author kongweiguang

use std::{ffi::OsString, time::Duration};

use kerminal_application::{
    AppError,
    agents::AgentTerminalSnapshotRequest,
    sessions::{
        CreateSessionRequest, PlainTextSnapshotRequest, SessionFilter, SessionRuntimeMetadata,
        logging::{SessionLogStartRequest, redact_terminal_text},
    },
};
use kerminal_domain::{
    identity::{AgentSessionId, HostId, PaneId, SessionId},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        SshInitialCommand, SshSessionTarget, TerminalSize,
    },
};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::McpServices;

const DEFAULT_COLUMNS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;
const DEFAULT_SNAPSHOT_BYTES: usize = 24 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateArgs {
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExplicitSessionArgs {
    session_id: String,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteArgs {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
    #[serde(default)]
    binding_generation: Option<u64>,
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotArgs {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResizeArgs {
    session_id: String,
    cols: u16,
    rows: u16,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogArgs {
    session_id: String,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReconnectArgs {
    pane_id: String,
    #[serde(default)]
    agent_session_id: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// 创建后台 session；默认 shell 经共享 ProfileService 解析，明确传入 shell 时保留调用方选择且不创建 Agent TUI。
///
/// @author kongweiguang
pub(crate) async fn create(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<CreateArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let target_name = args.target.as_deref().unwrap_or("local");
    if let Some(agent_session_id) = args.agent_session_id.as_deref()
        && AgentSessionId::parse(agent_session_id).is_err()
    {
        return failure(AppError::invalid("agent session id"));
    }
    let target = match target_name {
        "local" => {
            if args.host_id.is_some() {
                return failure(AppError::invalid("hostId is only valid for an SSH target"));
            }
            if args.cwd.as_ref().is_some_and(|cwd| cwd.trim().is_empty()) {
                return failure(AppError::invalid("cwd cannot be empty"));
            }
            let mut local_target = match args.shell {
                Some(shell) if shell.trim().is_empty() => {
                    return failure(AppError::invalid("shell cannot be empty"));
                }
                Some(shell) => LocalSessionTarget {
                    program: OsString::from(shell),
                    args: Vec::new(),
                    working_directory: None,
                    environment: Default::default(),
                },
                None => {
                    if cancellation.is_cancelled() {
                        return failure(AppError::cancelled("MCP request was cancelled"));
                    }
                    let profiles = match services.require_profiles() {
                        Ok(profiles) => profiles,
                        Err(error) => return failure(error),
                    };
                    match profiles.default_local_session_target().await {
                        Ok(target) => target,
                        Err(error) => return failure(error),
                    }
                }
            };
            if let Some(cwd) = args.cwd {
                local_target.working_directory = Some(cwd.into());
            }
            SessionTarget::Local(local_target)
        }
        "ssh" => {
            if args.shell.is_some() {
                return failure(AppError::invalid("shell is only valid for local targets"));
            }
            let Some(host_id) = args.host_id.as_deref() else {
                return failure(AppError::invalid("SSH target requires hostId"));
            };
            let host_id = match HostId::parse(host_id) {
                Ok(host_id) => host_id,
                Err(error) => {
                    tracing::debug!(%error, "MCP terminal.create received an invalid host id");
                    return failure(AppError::invalid("host id"));
                }
            };
            if args.cwd.as_ref().is_some_and(|cwd| cwd.trim().is_empty()) {
                return failure(AppError::invalid("cwd cannot be empty"));
            }
            SessionTarget::Ssh(SshSessionTarget {
                host_id,
                working_directory: args.cwd.map(Into::into),
                command: None,
            })
        }
        _ => return failure(AppError::invalid("target must be local or ssh")),
    };
    let size = match terminal_size(
        args.cols.unwrap_or(DEFAULT_COLUMNS),
        args.rows.unwrap_or(DEFAULT_ROWS),
    ) {
        Ok(size) => size,
        Err(error) => return failure(error),
    };
    match services
        .sessions
        .create_cancellable(
            CreateSessionRequest {
                target: target.clone(),
                owner: SessionOwner::Mcp,
                size,
            },
            cancellation,
        )
        .await
    {
        Ok(session) => {
            let session_id = session.session_id.to_string();
            let target_kind = match target {
                SessionTarget::Local(_) => "local",
                SessionTarget::Ssh(_) => "ssh",
                _ => "unsupported",
            };
            success(
                format!("已创建 headless {target_kind} PTY；可继续使用显式 sessionId。"),
                json!({
                    "sessionId": session_id,
                    "session": session_summary(services, &session).await,
                    "target": target_kind,
                    "hostId": args.host_id,
                    "headless": true,
                    "scope": "global",
                    "outputBuffered": true,
                    "ui": { "tabCreated": false, "paneCreated": false }
                }),
                vec![
                    json!({ "type": "terminalSession", "id": session_id, "headless": true, "target": target_kind }),
                ],
            )
        }
        Err(error) => failure(error),
    }
}

/// 原样提交 UTF-8 输入；历史资格和失败隔离由 application 用例处理，Agent endpoint 仍显式解析目标。
pub(crate) async fn write(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<WriteArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    if args.data.is_empty() {
        return failure(AppError::invalid("terminal input is empty"));
    }
    let session_id = match resolve_session_id(
        services,
        args.session_id.as_deref(),
        args.agent_session_id.as_deref(),
        args.binding_generation,
        cancellation.clone(),
    )
    .await
    {
        Ok(session_id) => session_id,
        Err(error) => return failure(error),
    };
    let bytes_written = args.data.len();
    let commands = match services.require_commands() {
        Ok(commands) => commands,
        Err(error) => return failure(error),
    };
    match commands
        .write_mcp_terminal_input(session_id, args.data, cancellation)
        .await
    {
        Ok(receipt) => success(
            if receipt.history_error_code.is_some() {
                "已向终端写入输入字节；命令历史未能保存。".to_owned()
            } else {
                "已向终端写入输入字节。".to_owned()
            },
            json!({ "bytesWritten": bytes_written }),
            Vec::new(),
        ),
        Err(error) => failure(error),
    }
}

/// 返回终端引擎的有界纯文本快照；不会从活动 Tab 猜测 Agent 的输出来源。
pub(crate) async fn snapshot(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<SnapshotArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let agent_session_id = match resolve_agent_scope(services, args.agent_session_id.as_deref()) {
        Ok(agent_session_id) => agent_session_id,
        Err(error) => return failure(error),
    };
    if let Some(agent_session_id) = &agent_session_id
        && let Err(error) =
            ensure_live_agent_scope(services, agent_session_id.clone(), cancellation.clone()).await
    {
        return failure(error);
    }
    let session_id = match resolve_session_id(
        services,
        args.session_id.as_deref(),
        args.agent_session_id.as_deref(),
        None,
        cancellation.clone(),
    )
    .await
    {
        Ok(session_id) => session_id,
        Err(error) => return failure(error),
    };
    let session = match services.sessions.list(SessionFilter::default()).await {
        Ok(sessions) => match sessions
            .into_iter()
            .find(|session| session.session_id == session_id)
        {
            Some(session) => session,
            None => return failure(AppError::not_found("Terminal session not found")),
        },
        Err(error) => return failure(error),
    };
    let max_bytes = args.max_bytes.unwrap_or(DEFAULT_SNAPSHOT_BYTES);
    if max_bytes == 0 || max_bytes > MAX_SNAPSHOT_BYTES {
        return failure(AppError::invalid("maxBytes must be between 1 and 1048576"));
    }
    let snapshot = match services
        .sessions
        .plaintext_snapshot(
            session_id.clone(),
            PlainTextSnapshotRequest {
                include_scrollback: true,
                max_lines: 2000,
                max_bytes: Some(max_bytes),
            },
        )
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return failure(error),
    };
    let captured_bytes = snapshot.text.len();
    let (safe_output, redacted) = redact_terminal_text(&snapshot.text);
    let mut safe_output = safe_output.to_string();
    let mut truncated = snapshot.truncated;
    truncate_utf8_to_bytes(&mut safe_output, max_bytes, &mut truncated);
    if let Some(agent_session_id) = agent_session_id {
        let request = AgentTerminalSnapshotRequest {
            agent_session_id,
            target_session_id: session_id.clone(),
            output: safe_output.clone(),
            max_bytes,
            truncated,
            redacted,
        };
        let agent_service = match services.agent_service() {
            Ok(service) => service,
            Err(error) => return failure(error),
        };
        if let Err(error) = agent_service
            .record_terminal_snapshot(request, cancellation.clone())
            .await
        {
            return failure(error);
        }
    }
    success(
        format!(
            "已读取终端最近 {captured_bytes} 字节输出{}。",
            if truncated { "（已截断）" } else { "" }
        ),
        json!({
            "session": session_summary(services, &session).await,
            "snapshot": {
                "data": safe_output,
                "capturedBytes": captured_bytes,
                "maxBytes": max_bytes,
                "truncated": truncated,
                "redacted": redacted
            }
        }),
        vec![json!({ "type": "terminalSession", "id": session_id.as_str() })],
    )
}

/// 脱敏可能使标记变长；结果仍必须遵守调用方的严格 UTF-8 字节预算。
fn truncate_utf8_to_bytes(value: &mut String, max_bytes: usize, truncated: &mut bool) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    *truncated = true;
}

/// 调整指定 session 的 PTY 网格；Agent 路由 scope 必须与显式 ID 一致，但不限制可操作的全局终端。
pub(crate) async fn resize(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<ResizeArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let agent_id = match resolve_agent_scope(services, args.agent_session_id.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(error) => return failure(error),
    };
    if let Some(agent_id) = agent_id
        && let Err(error) = ensure_live_agent_scope(services, agent_id, cancellation.clone()).await
    {
        return failure(error);
    }
    let session_id = match SessionId::parse(&args.session_id) {
        Ok(session_id) => session_id,
        Err(error) => {
            tracing::debug!(%error, "MCP terminal.resize received an invalid session id");
            return failure(AppError::invalid("session id"));
        }
    };
    let size = match terminal_size(args.cols, args.rows) {
        Ok(size) => size,
        Err(error) => return failure(error),
    };
    match services.sessions.resize(session_id, size).await {
        Ok(()) => success(
            format!("终端尺寸已调整为 {}x{}。", args.cols, args.rows),
            json!({ "cols": args.cols, "rows": args.rows }),
            Vec::new(),
        ),
        Err(error) => failure(error),
    }
}

/// 关闭显式 session 并等待 application service 回收 transport；Agent 路由只核对会话上下文，不当作目标权限边界。
pub(crate) async fn close(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<ExplicitSessionArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let agent_id = match resolve_agent_scope(services, args.agent_session_id.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(error) => return failure(error),
    };
    if let Some(agent_id) = agent_id
        && let Err(error) = ensure_live_agent_scope(services, agent_id, cancellation.clone()).await
    {
        return failure(error);
    }
    let session_id = match SessionId::parse(&args.session_id) {
        Ok(session_id) => session_id,
        Err(error) => {
            tracing::debug!(%error, "MCP terminal.close received an invalid session id");
            return failure(AppError::invalid("session id"));
        }
    };
    match services.sessions.close(session_id.clone()).await {
        Ok(()) => success(
            "终端会话已关闭。".to_owned(),
            json!({ "sessionId": session_id.as_str(), "closed": true }),
            vec![json!({ "type": "terminalSession", "id": session_id.as_str() })],
        ),
        Err(error) => failure(error),
    }
}

/// 将日志录制状态映射到独立日志用例，并要求每次操作都关联实际存在的显式 session。
pub(crate) async fn log(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<LogArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let session_id = match SessionId::parse(&args.session_id) {
        Ok(session_id) => session_id,
        Err(_) => return failure(AppError::invalid("session id")),
    };
    let agent_id = match resolve_agent_scope(services, args.agent_session_id.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(error) => return failure(error),
    };
    if let Some(agent_id) = agent_id
        && let Err(error) = ensure_live_agent_scope(services, agent_id, cancellation.clone()).await
    {
        return failure(error);
    }
    let session = match services.sessions.list(SessionFilter::default()).await {
        Ok(sessions) => sessions
            .into_iter()
            .find(|session| session.session_id == session_id),
        Err(error) => return failure(error),
    };
    let Some(session) = session else {
        return failure(AppError::not_found("Terminal session not found"));
    };
    let service = match services.require_session_logs() {
        Ok(service) => service,
        Err(error) => return failure(error),
    };
    let state = match tool_id {
        "terminal.log.start" => {
            let (shell, cwd) = match &session.target {
                SessionTarget::Local(target) => (
                    target.program.to_string_lossy().into_owned(),
                    target
                        .working_directory
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                ),
                SessionTarget::Ssh(target) => (
                    "ssh".to_owned(),
                    target
                        .working_directory
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                ),
                SessionTarget::Telnet(_) => ("telnet".to_owned(), None),
                SessionTarget::Serial(_) => ("serial".to_owned(), None),
                SessionTarget::Container(_) => ("container".to_owned(), None),
            };
            service
                .start(
                    cancellation,
                    SessionLogStartRequest {
                        session_id: session_id.clone(),
                        shell,
                        cwd,
                    },
                )
                .await
        }
        "terminal.log.stop" => service.stop(cancellation, session_id.clone()).await,
        "terminal.log.state" => service.state(cancellation, session_id.clone()).await,
        _ => return failure(AppError::invalid("Unknown terminal log tool")),
    };
    match state {
        Ok(state) => {
            let value = match serde_json::to_value(&state) {
                Ok(value) => value,
                Err(error) => {
                    return failure(AppError::infrastructure_with_source(
                        "Terminal log state serialization failed",
                        error,
                    ));
                }
            };
            success(
                match tool_id {
                    "terminal.log.start" => "终端日志已启动。".to_owned(),
                    "terminal.log.stop" => "终端日志已停止。".to_owned(),
                    _ => "已读取终端日志状态。".to_owned(),
                },
                json!({ "sessionId": session_id.as_str(), "state": value }),
                vec![json!({ "type": "terminalSession", "id": session_id.as_str() })],
            )
        }
        Err(error) => failure(error),
    }
}

/// 列出 runtime 中真实存在的会话；Agent scope 只校验自身存活，不缩小 global session 能力。
pub(crate) async fn list(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<ListArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let agent_id = match resolve_agent_scope(services, args.agent_session_id.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(error) => return failure(error),
    };
    if let Some(agent_id) = agent_id
        && let Err(error) = ensure_live_agent_scope(services, agent_id, cancellation.clone()).await
    {
        return failure(error);
    }
    let sessions = match services.sessions.list(SessionFilter::default()).await {
        Ok(sessions) if !cancellation.is_cancelled() => sessions,
        Ok(_) => return failure(AppError::cancelled("MCP request was cancelled")),
        Err(error) => return failure(error),
    };
    let mut summaries = Vec::new();
    for session in sessions.iter().filter(|session| {
        session.lifecycle == SessionLifecycle::Running
            && !matches!(session.owner, SessionOwner::Agent { .. })
    }) {
        if cancellation.is_cancelled() {
            return failure(AppError::cancelled("MCP request was cancelled"));
        }
        summaries.push(session_summary(services, session).await);
    }
    let count = summaries.len();
    let entities = summaries
        .iter()
        .filter_map(|summary| {
            summary
                .get("id")
                .and_then(Value::as_str)
                .map(|id| json!({ "type": "terminalSession", "id": id }))
        })
        .collect();
    success(
        format!("当前全局 scope 包含 {count} 个用户终端。"),
        json!({
            "sessionCount": count,
            "sessions": summaries.clone(),
            "scope": { "kind": "global" },
            "terminals": summaries
        }),
        entities,
    )
}

/// 依据显式 pane 身份重连唯一已有会话；不查找其它 pane 或将失败目标改绑到当前 Tab。
pub(crate) async fn reconnect(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let args = match parse_args::<ReconnectArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let agent_id = match resolve_agent_scope(services, args.agent_session_id.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(error) => return failure(error),
    };
    if let Some(agent_id) = agent_id
        && let Err(error) = ensure_live_agent_scope(services, agent_id, cancellation.clone()).await
    {
        return failure(error);
    }
    let pane_id = match PaneId::parse(&args.pane_id) {
        Ok(pane_id) => pane_id,
        Err(_) => return failure(AppError::invalid("pane id")),
    };
    let timeout_ms = args.timeout_ms.unwrap_or(30_000);
    if !(1..=60_000).contains(&timeout_ms) {
        return failure(AppError::invalid("timeoutMs must be between 1 and 60000"));
    }
    let sessions = match services.sessions.list(SessionFilter::default()).await {
        Ok(sessions) if !cancellation.is_cancelled() => sessions,
        Ok(_) => return failure(AppError::cancelled("MCP request was cancelled")),
        Err(error) => return failure(error),
    };
    let Some(session) = sessions.iter().find(|session| {
        matches!(&session.owner, SessionOwner::Ui { pane_id: owner, .. } if owner == &pane_id)
    }) else {
        return failure(AppError::not_found("The requested terminal pane does not exist"));
    };
    if session.lifecycle == SessionLifecycle::Running {
        return failure(AppError::conflict(
            "The requested terminal pane is already connected",
        ));
    }
    if !matches!(
        session.lifecycle,
        SessionLifecycle::Exited | SessionLifecycle::Failed
    ) {
        return failure(AppError::conflict(
            "The requested terminal pane is not reconnectable",
        ));
    }
    let session_id = session.session_id.clone();
    let reconnect_cancellation = cancellation.child_token();
    let mut reconnect = Box::pin(
        services
            .sessions
            .reconnect_cancellable(session_id, reconnect_cancellation.clone()),
    );
    let result = tokio::select! {
        result = reconnect.as_mut() => result,
        _ = cancellation.cancelled() => {
            reconnect_cancellation.cancel();
            let _ = reconnect.as_mut().await;
            return failure(AppError::cancelled("MCP request was cancelled"));
        }
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            reconnect_cancellation.cancel();
            let _ = reconnect.as_mut().await;
            return failure(AppError::timeout("Terminal reconnect timed out"));
        }
    };
    match result {
        Ok(session) => {
            let summary = session_summary(services, &session).await;
            success(
                "已重连指定终端 pane。".to_owned(),
                json!({ "paneId": pane_id.as_str(), "session": summary }),
                vec![json!({ "type": "terminalSession", "id": session.session_id.as_str() })],
            )
        }
        Err(error) => failure(error),
    }
}

/// 合并路由 scope 与显式 Agent ID；不同身份不得把会话查询扩展到另一 Agent。
fn resolve_agent_scope(
    services: &McpServices,
    explicit_id: Option<&str>,
) -> Result<Option<AgentSessionId>, AppError> {
    let explicit = explicit_id
        .map(AgentSessionId::parse)
        .transpose()
        .map_err(|_| AppError::invalid("agent session id"))?;
    match (&services.agent_scope, explicit) {
        (Some(scoped), Some(explicit)) if *scoped != explicit => Err(AppError::conflict(
            "Agent endpoint session does not match agentSessionId",
        )),
        (Some(scoped), _) => Ok(Some(scoped.clone())),
        (None, Some(explicit)) => Ok(Some(explicit)),
        (None, None) => Ok(None),
    }
}

/// 接受 Agent scope 前复核会话状态；未确认恢复仍可进入查询并显式返回原 owner 状态。
pub(crate) async fn ensure_live_agent_scope(
    services: &McpServices,
    agent_id: AgentSessionId,
    cancellation: CancellationToken,
) -> Result<(), AppError> {
    let view = services
        .agent_service()?
        .get(agent_id, cancellation)
        .await?;
    if view.session.status != kerminal_domain::agents::AgentSessionStatus::Active
        || (view.resume_recovery.is_none()
            && !view
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running))
    {
        return Err(AppError::conflict("Agent session is no longer active"));
    }
    Ok(())
}

/// 汇总运行态 session 的稳定字段，不序列化内部 transport 或实现状态。
pub(crate) async fn session_summary(services: &McpServices, session: &SessionInfo) -> Value {
    // metadata 查询来自 session registry；失败时省略未知细节，不能把查询故障误报成集成已禁用。
    let runtime = match services
        .sessions
        .runtime_metadata(session.session_id.clone())
        .await
    {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            tracing::debug!(%error, session_id = %session.session_id, "terminal runtime metadata is unavailable");
            None
        }
    };
    let (owner, headless) = match &session.owner {
        SessionOwner::Ui { .. } => ("ui", false),
        SessionOwner::Agent { .. } => ("agent", true),
        SessionOwner::Mcp => ("mcp", true),
        SessionOwner::Service { .. } => ("service", true),
    };
    let lifecycle = match session.lifecycle {
        SessionLifecycle::Starting => "starting",
        SessionLifecycle::Running => "running",
        SessionLifecycle::Closing => "closing",
        SessionLifecycle::Exited => "exited",
        SessionLifecycle::Closed => "closed",
        SessionLifecycle::Failed => "failed",
    };
    let (shell, cwd) = match &session.target {
        SessionTarget::Local(target) => (
            target.program.to_string_lossy().into_owned(),
            target
                .working_directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        ),
        SessionTarget::Ssh(target) => (
            "ssh".to_owned(),
            target
                .working_directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        ),
        SessionTarget::Telnet(_) => ("telnet".to_owned(), None),
        SessionTarget::Serial(target) => (target.device.to_string_lossy().into_owned(), None),
        SessionTarget::Container(target) => {
            (format!("{:?}", target.runtime).to_ascii_lowercase(), None)
        }
    };
    let target_ref = match &session.target {
        SessionTarget::Ssh(target) => Some(target.host_id.as_str()),
        SessionTarget::Container(target) => target.host_id.as_ref().map(|host_id| host_id.as_str()),
        _ => None,
    };
    let target = match &session.target {
        SessionTarget::Local(_) => json!({
            "type": "local",
            "shell": shell,
            "cwd": cwd
        }),
        SessionTarget::Ssh(target) => json!({
            "type": "ssh",
            "hostId": target.host_id.as_str(),
            "command": ssh_initial_command_value(target.command.as_ref()),
            "cwd": cwd
        }),
        SessionTarget::Telnet(_) => json!({ "type": "telnet" }),
        SessionTarget::Serial(target) => json!({
            "type": "serial",
            "device": target.device.to_string_lossy(),
        }),
        SessionTarget::Container(target) => json!({
            "type": "container",
            "hostId": target.host_id.as_ref().map(|id| id.as_str()),
            "runtime": format!("{:?}", target.runtime).to_ascii_lowercase(),
            "containerId": target.container_id
        }),
    };
    let status = if session.lifecycle == SessionLifecycle::Running {
        "running"
    } else {
        "exited"
    };
    let agent_session_id = match &session.owner {
        SessionOwner::Agent { agent_session_id } => Some(agent_session_id.as_str()),
        _ => None,
    };
    let mut summary = json!({
        "id": session.session_id.as_str(),
        "sessionId": session.session_id.as_str(),
        "shell": shell,
        "cwd": cwd,
        "pid": runtime.and_then(|metadata| metadata.process_id),
        "targetRef": target_ref,
        "targetToken": null,
        "shellIntegration": runtime.map(|metadata| session_shell_integration(session, metadata)),
        "target": target,
        "owner": owner,
        "cols": session.size.columns(),
        "rows": session.size.rows(),
        "status": status,
        "lifecycle": lifecycle,
        "connectionState": if status == "running" { "connected" } else { "disconnected" },
        "headless": headless
    });
    if let Some(agent_session_id) = agent_session_id {
        summary["agentSessionId"] = json!(agent_session_id);
    }
    summary
}

/// 将结构化 SSH 初始命令映射为稳定 wire 值，避免领域命令类型依赖 serde。
fn ssh_initial_command_value(command: Option<&SshInitialCommand>) -> Value {
    match command {
        None => Value::Null,
        Some(SshInitialCommand::Shell(script)) => json!({
            "kind": "shell",
            "script": script
        }),
        Some(SshInitialCommand::Process { program, args }) => json!({
            "kind": "process",
            "program": program,
            "args": args
        }),
    }
}

/// 按 transport 报告的 launch hook 与实际 OSC 状态保留旧终端摘要语义。
fn session_shell_integration(session: &SessionInfo, metadata: SessionRuntimeMetadata) -> Value {
    let enabled = metadata.shell_integration_enabled;
    let reason = if enabled || metadata.shell_integration_observed || metadata.prompt_ready {
        None
    } else {
        Some("Shell integration is not enabled for this session")
    };
    let shell = match &session.target {
        SessionTarget::Local(target) if enabled => target
            .program
            .to_string_lossy()
            .rsplit(['\\', '/'])
            .next()
            .map(str::to_owned),
        _ => None,
    };
    json!({
        "status": if enabled { "enabled" } else { "disabled" },
        "shell": shell,
        "scriptPath": Value::Null,
        "reason": reason
    })
}

/// 将参数映射错误降为不含原始 JSON、命令或 secret 的稳定公开应用错误。
pub(crate) fn parse_args<T: for<'de> Deserialize<'de>>(
    arguments: Map<String, Value>,
) -> Result<T, AppError> {
    serde_json::from_value(Value::Object(arguments)).map_err(|error| {
        tracing::debug!(%error, "invalid MCP tool argument shape");
        AppError::invalid("tool arguments")
    })
}

/// 解析必须显式传递的 session id；MCP adapter 不从 UI 选中项猜测目标。
fn parse_session_id(value: Option<&str>) -> Result<SessionId, AppError> {
    let value = value.ok_or_else(|| AppError::invalid("sessionId is required"))?;
    SessionId::parse(value).map_err(|error| {
        tracing::debug!(%error, "MCP terminal tool received an invalid session id");
        AppError::invalid("session id")
    })
}

/// 创建非零的终端网格尺寸并拒绝超出协议字段宽度的请求。
fn terminal_size(columns: u16, rows: u16) -> Result<TerminalSize, AppError> {
    TerminalSize::new(columns, rows, 0, 0).map_err(|_| AppError::invalid("terminal size"))
}

/// 生成既兼容旧 MCP schema 又携带稳定 success/error 字段的结构化成功响应。
pub(crate) fn success(summary: String, data: Value, entities: Vec<Value>) -> CallToolResult {
    CallToolResult::structured(json!({
        "summary": summary,
        "data": data,
        "entities": entities,
        "nextHints": []
    }))
}

/// 映射应用错误为 MCP 可见的稳定错误码；不泄露底层错误文本或凭据。
pub(crate) fn failure(error: AppError) -> CallToolResult {
    failure_with_data(error, json!({}))
}

/// 生成带有可恢复查询标识的结构化错误，同时沿用 MCP 的固定安全错误外形。
pub(crate) fn failure_with_data(error: AppError, data: Value) -> CallToolResult {
    let code = error.public_code();
    let recoverable = matches!(
        code,
        kerminal_application::PublicErrorCode::Conflict
            | kerminal_application::PublicErrorCode::Infrastructure
    ) && code != kerminal_application::PublicErrorCode::OutcomeUnknown;
    let message = error.public_message();
    CallToolResult::structured_error(json!({
        "summary": message,
        "error": message,
        "data": data,
        "entities": [],
        "errorKind": code.as_str(),
        "recoverable": recoverable,
        "nextHints": if code == kerminal_application::PublicErrorCode::OutcomeUnknown {
            vec!["先查询或读回受影响资源；确认操作未生效后再决定是否重新提交。"]
        } else if recoverable {
            vec!["确认目标状态后再重试。"]
        } else {
            Vec::<&str>::new()
        }
    }))
}

#[cfg(test)]
#[path = "../tests/unit/terminal_command_wire.rs"]
mod terminal_command_wire_tests;

/// 显式 session id 始终优先；仅内置 Agent endpoint 可省略 id 并解析其首选绑定。
async fn resolve_session_id(
    services: &McpServices,
    explicit_session_id: Option<&str>,
    explicit_agent_session_id: Option<&str>,
    binding_generation: Option<u64>,
    cancellation: CancellationToken,
) -> Result<SessionId, AppError> {
    let explicit_agent = explicit_agent_session_id
        .map(AgentSessionId::parse)
        .transpose()
        .map_err(|_| AppError::invalid("agent session id"))?;
    if let (Some(scoped), Some(explicit)) = (&services.agent_scope, &explicit_agent)
        && scoped != explicit
    {
        return Err(AppError::conflict(
            "Agent endpoint session does not match agentSessionId",
        ));
    }
    if let Some(session_id) = explicit_session_id {
        return parse_session_id(Some(session_id));
    }
    let agent_session_id = match (&services.agent_scope, explicit_agent) {
        (Some(scoped), _) => scoped.clone(),
        (None, Some(explicit)) => explicit,
        (None, None) => return Err(AppError::invalid("sessionId is required")),
    };
    let agent_service = services.agent_service()?;
    let resolution = agent_service
        .resolve_preferred_target(agent_session_id, cancellation)
        .await?;
    if binding_generation.is_some_and(|requested| requested != resolution.binding.generation) {
        return Err(AppError::conflict(
            "Agent terminal binding generation has changed",
        ));
    }
    resolution
        .resolved_session_id
        .ok_or_else(|| AppError::conflict("Agent session has no live preferred terminal"))
}
