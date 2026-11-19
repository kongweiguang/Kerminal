//! MCP 终端工具到 SessionService 的显式适配。
//!
//! @author kongweiguang

use std::ffi::OsString;

use kerminal_application::{
    AppError, AppResult,
    agents::AgentTerminalSnapshotRequest,
    plugins::capabilities,
    sessions::{
        CreateSessionRequest, PlainTextSnapshotRequest, SessionFilter, SessionRuntimeMetadata,
        logging::redact_terminal_text,
    },
};
use kerminal_domain::{
    identity::{AgentSessionId, HostId, SessionId},
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
    // 本地目标不依赖 SSH；只有解析出 SSH 主机后才追加请求级门禁，避免禁用远程插件影响本地终端。
    let target_units: &[&str] = match &target {
        SessionTarget::Ssh(_) => capabilities::host_protocol_units("ssh"),
        _ => &[],
    };
    if let Some(result) = units_gate(services, "terminal.create", target_units) {
        return result;
    }
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
                initial_palette: None,
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
///
/// 右栏 Agent 的显式目标先按真实 owner 判定：只有 Running 的 MCP headless 会话
/// 可以绕过路由设置，因为它由 terminal.create 为后台执行创建，不存在改写用户可见
/// 终端的风险。其余显式目标和隐式首选目标都必须先读到已提交设置；设置缺失或不可读
/// 时 fail closed，不能把生产装配缺项静默当成“可见终端”放行。
///
/// @author kongweiguang
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
    let scoped = services.agent_scope.is_some();
    // 显式 scoped 目标先查真实会话，使已确认安全的 headless 写入不受设置读取故障影响；
    // 同时避免在后台模式下先解析首选绑定、再发现目标是用户可见终端。
    if scoped && let Some(explicit_session_id) = args.session_id.as_deref() {
        let session_id = match resolve_session_id(
            services,
            Some(explicit_session_id),
            args.agent_session_id.as_deref(),
            args.binding_generation,
            cancellation.clone(),
        )
        .await
        {
            Ok(session_id) => session_id,
            Err(error) => return failure(error),
        };
        let session = match find_session(services, &session_id).await {
            Ok(session) => session,
            Err(error) => return failure(error),
        };
        if session.as_ref().is_some_and(is_running_mcp_session) {
            if let Some(result) =
                session_target_gate_for(services, "terminal.write", session.as_ref())
            {
                return result;
            }
            return write_terminal_input(services, session_id, args.data, cancellation).await;
        }
        let routing = match require_agent_command_routing(services).await {
            Ok(routing) => routing,
            Err(result) => return *result,
        };
        if routing == AgentCommandRouting::Background {
            let Some(session) = session else {
                return agent_background_session_missing();
            };
            // 已退出或正在关闭的会话即使 owner 正确也不能继续写入；让调用方重新 create，
            // 而不是把 application 的通用 not-found 当成可以回退到其它终端的信号。
            if session.lifecycle != SessionLifecycle::Running {
                return agent_background_session_missing();
            }
            // 后台模式只接受 terminal.create 创建的 Running MCP headless 会话；
            // Agent 自身的 CLI、UI pane 和内部 service 会话都不是命令落点。
            return agent_write_requires_explicit_headless(ssh_command_available(services));
        }
        if let Some(result) = session_target_gate_for(services, "terminal.write", session.as_ref())
        {
            return result;
        }
        return write_terminal_input(services, session_id, args.data, cancellation).await;
    }
    // 隐式 scoped 写入必须在解析首选绑定之前确认路由，避免后台模式触碰用户可见终端。
    // 全局 endpoint 即使携带显式 agentSessionId 也保持原行为，不受该开关约束。
    if scoped {
        let routing = match require_agent_command_routing(services).await {
            Ok(routing) => routing,
            Err(result) => return *result,
        };
        if routing == AgentCommandRouting::Background {
            return agent_write_requires_explicit_headless(ssh_command_available(services));
        }
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
    if let Some(result) = session_target_gate(services, "terminal.write", &session_id).await {
        return result;
    }
    write_terminal_input(services, session_id, args.data, cancellation).await
}

/// 统一执行实际写入，避免路由分支各自复制字节计数和错误映射。
async fn write_terminal_input(
    services: &McpServices,
    session_id: SessionId,
    data: String,
    cancellation: CancellationToken,
) -> CallToolResult {
    let bytes_written = data.len();
    let commands = match services.require_commands() {
        Ok(commands) => commands,
        Err(error) => return failure(error),
    };
    match commands
        .write_mcp_terminal_input(session_id, data, cancellation)
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
    if let Some(result) = session_target_gate(services, "terminal.snapshot", &session_id).await {
        return result;
    }
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
    if let Some(result) = session_target_gate(services, "terminal.resize", &session_id).await {
        return result;
    }
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
    // 关闭属于资源回收路径：即使该会话依赖的插件已被禁用也必须允许释放，避免遗留 PTY/transport。
    match services.sessions.close(session_id.clone()).await {
        Ok(()) => success(
            "终端会话已关闭。".to_owned(),
            json!({ "sessionId": session_id.as_str(), "closed": true }),
            vec![json!({ "type": "terminalSession", "id": session_id.as_str() })],
        ),
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

/// 检查工具目录可见性；未注入 registry 的离线/测试装配按全部启用处理。
///
/// 使用 `tool_visibility_gate` 而不是 `tool_units`，让 any-of 凭据工具在整族禁用时
/// 仍返回完整的候选/禁用单元，而不是空 requiredUnits 的歧义拒绝。
///
/// 生产 `validate_complete` 已要求注入 registry，因此这里的宽松分支只服务离线调用方。
///
/// @author kongweiguang
pub(crate) fn plugin_gate(services: &McpServices, tool: &str) -> Option<CallToolResult> {
    let registry = services.plugins.as_deref()?;
    let (required_units, disabled_units) = capabilities::tool_visibility_gate(registry, tool)?;
    Some(plugin_disabled(tool, required_units, &disabled_units))
}

/// 对已解析出的真实目标追加单元要求；只有实际禁用时才生成结构化拒绝。
///
/// 空集合直接放行，避免核心工具因空映射被误判为未注册。
///
/// @author kongweiguang
pub(crate) fn units_gate(
    services: &McpServices,
    tool: &str,
    required_units: &[&str],
) -> Option<CallToolResult> {
    if required_units.is_empty() {
        return None;
    }
    let registry = services.plugins.as_deref()?;
    let disabled_units = required_units
        .iter()
        .copied()
        .filter(|unit| !registry.is_enabled(unit))
        .collect::<Vec<_>>();
    (!disabled_units.is_empty()).then(|| plugin_disabled(tool, required_units, &disabled_units))
}

/// 生成稳定的 `plugin_disabled` 业务结果，复用其余失败分支的 JSON 外形。
///
/// 不复用 `PublicErrorCode` 变体，避免为一个目录层状态扩散全仓匹配；
/// 调用方拿到结构化错误后可区分“工具未知”（协议错误）与“能力被关闭”。
///
/// @author kongweiguang
pub(crate) fn plugin_disabled(
    tool: &str,
    required_units: &[&str],
    disabled_units: &[&str],
) -> CallToolResult {
    let unit_list = disabled_units.join("、");
    let message = format!("工具 {tool} 依赖已禁用的插件：{unit_list}。请在扩展页启用后重试。");
    CallToolResult::structured_error(json!({
        "summary": message,
        "error": message,
        "data": {
            "toolId": tool,
            "requiredUnits": required_units,
            "disabledUnits": disabled_units,
        },
        "entities": [],
        "errorKind": "plugin_disabled",
        "recoverable": false,
        "nextHints": [format!("在扩展管理页启用 {unit_list} 后重试。")]
    }))
}

/// 按会话既有目标追加协议门禁，避免禁用某协议后仍能写入、读取或调整其会话。
///
/// Telnet 与串口各有独立启停单元，不能只检查 SSH；否则关闭 Telnet 后
/// 仍可通过 write/snapshot/resize 操作既有会话。Container 会话保持不门禁：
/// 已有本地容器会话仍需要可回收和观察，禁用插件不阻断既有资源。
///
/// 找不到会话或读取失败时不在这里伪造结论，交给原操作返回其稳定错误；
/// 关闭与列表不经过此门禁，保证禁用插件后仍可回收资源。
///
/// @author kongweiguang
async fn session_target_gate(
    services: &McpServices,
    tool: &str,
    session_id: &SessionId,
) -> Option<CallToolResult> {
    let registry = services.plugins.as_deref()?;
    // 常见路径下三个协议都启用，无需查询会话即可放行，避免给每次 write 增加列表开销。
    if registry.is_enabled(capabilities::SSH_UNIT)
        && registry.is_enabled(capabilities::TELNET_UNIT)
        && registry.is_enabled(capabilities::SERIAL_UNIT)
    {
        return None;
    }
    let sessions = services
        .sessions
        .list(SessionFilter::default())
        .await
        .ok()?;
    let session = sessions
        .iter()
        .find(|session| &session.session_id == session_id)?;
    session_target_gate_for(services, tool, Some(session))
}

/// 复用调用方已读取的会话执行协议门禁，后台路由的 owner 判定不重复 list。
///
/// @author kongweiguang
fn session_target_gate_for(
    services: &McpServices,
    tool: &str,
    session: Option<&SessionInfo>,
) -> Option<CallToolResult> {
    let registry = services.plugins.as_deref()?;
    if registry.is_enabled(capabilities::SSH_UNIT)
        && registry.is_enabled(capabilities::TELNET_UNIT)
        && registry.is_enabled(capabilities::SERIAL_UNIT)
    {
        return None;
    }
    let session = session?;
    let required_units: &[&str] = match &session.target {
        SessionTarget::Ssh(_) => capabilities::host_protocol_units("ssh"),
        SessionTarget::Telnet(_) => capabilities::host_protocol_units("telnet"),
        SessionTarget::Serial(_) => capabilities::host_protocol_units("serial"),
        SessionTarget::Local(_) | SessionTarget::Container(_) => &[],
    };
    units_gate(services, tool, required_units)
}

/// Agent 终端命令的当前路由；与设置页共用同一已提交设置，不建立第二份运行态缓存。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentCommandRouting {
    /// 写入首选可见终端，保持 0.4 之前的交互。
    Visible,
    /// 只允许显式 headless 会话，避免后台 Agent 改写用户正在看的终端。
    Background,
}

/// 读取已提交设置；未注入设置仓储时返回 None，保持旧 fixture 的空 nextHints 契约。
///
/// 真实读取失败必须上抛，不能静默把损坏或不可读配置当成“可见终端”继续写入。
///
/// @author kongweiguang
async fn stored_agent_command_routing(
    services: &McpServices,
) -> AppResult<Option<AgentCommandRouting>> {
    let Some(preferences) = services.preferences.as_ref() else {
        return Ok(None);
    };
    let loaded = preferences.load_settings().await?;
    Ok(Some(
        if loaded.settings.terminal.agent_commands_in_terminal {
            AgentCommandRouting::Visible
        } else {
            AgentCommandRouting::Background
        },
    ))
}

/// 写入路径需要确定的策略；缺失设置仓储时 fail closed。
///
/// 生产组合根必须注入 preferences，只有离线 fixture 会缺省。显式 Running MCP
/// headless 目标在调用本函数前已经放行，因此这里的拒绝不会阻断安全的后台会话；
/// 其余隐式或可见目标拿不到路由时宁可失败，也不能默认按可见终端写入。
/// 仅在失败路径分配完整 MCP 错误响应，保持既有响应结构和提示。
///
/// @author kongweiguang
async fn require_agent_command_routing(
    services: &McpServices,
) -> Result<AgentCommandRouting, Box<CallToolResult>> {
    match stored_agent_command_routing(services).await {
        Ok(Some(routing)) => Ok(routing),
        Ok(None) => Err(Box::new(agent_routing_settings_unavailable())),
        Err(error) => Err(Box::new(failure(error))),
    }
}

/// 设置仓储缺失时拒绝依赖路由的写入，避免生产装配缺项被静默当成可见终端。
///
/// 替代路径仍是 terminal.create 创建的显式 headless 会话；该路径不读取本设置。
///
/// @author kongweiguang
fn agent_routing_settings_unavailable() -> CallToolResult {
    failure_with_data(
        AppError::unsupported(
            "Agent command routing settings are unavailable; create a headless session with terminal.create and pass its explicit sessionId",
        ),
        json!({
            "setting": "terminal.agentCommandsInTerminal",
            "required": "explicitHeadlessSessionId",
            "route": "unavailable",
            "fallback": "terminal.create"
        }),
    )
}

/// 判断显式目标是否为 terminal.create 创建的 Running headless 会话。
///
/// 只接受 MCP owner：UI pane 是用户正在观看的终端，Agent 自身的 CLI 是助手进程，
/// Service 会话属于内部编排目标；即使它们都是 headless，也不能作为右栏命令落点。
///
/// @author kongweiguang
fn is_running_mcp_session(session: &SessionInfo) -> bool {
    session.lifecycle == SessionLifecycle::Running && matches!(session.owner, SessionOwner::Mcp)
}

/// 用现有会话列表读取目标；不在 adapter 内维护第二份 session registry。
///
/// @author kongweiguang
async fn find_session(
    services: &McpServices,
    session_id: &SessionId,
) -> AppResult<Option<SessionInfo>> {
    Ok(services
        .sessions
        .list(SessionFilter::default())
        .await?
        .into_iter()
        .find(|session| &session.session_id == session_id))
}

/// 后台模式下拒绝可见终端写入，并给出可执行的替代路径。
///
/// SSH 替代路径只在 ssh.command 通过插件门禁时出现；关闭 SSH 后仍然只提示
/// terminal.create，避免调用方按提示调用一个已被目录隐藏的工具。两条文案都保持
/// `&'static str`，以沿用稳定的 conflict 错误构造而不拼接用户输入。
///
/// @author kongweiguang
fn agent_write_requires_explicit_headless(ssh_available: bool) -> CallToolResult {
    let message = if ssh_available {
        "Agent command routing is set to background; use terminal.create with an explicit headless sessionId, or ssh.command for non-interactive remote work"
    } else {
        "Agent command routing is set to background; use terminal.create with an explicit headless sessionId"
    };
    failure_with_data(
        AppError::conflict(message),
        json!({
            "setting": "terminal.agentCommandsInTerminal",
            "required": "explicitHeadlessSessionId",
            "route": "background"
        }),
    )
}

/// 判断 ssh.command 是否通过当前插件门禁；目录隐藏时提示不能把它当作备用路径。
///
/// 未注入 registry 的离线装配按可用处理，与插件门禁工具的宽松分支保持一致。
fn ssh_command_available(services: &McpServices) -> bool {
    services.plugins.as_deref().is_none_or(|registry| {
        capabilities::tool_visibility_gate(registry, "ssh.command").is_none()
    })
}

/// 后台模式下显式 session 已消失或退出时，在 writer 前给出可恢复的替代路径。
///
/// 这里使用 conflict 而不是把 application 的 not-found 原样透出，是因为调用方真正
/// 需要知道的是“不要回退到可见终端，重新创建一个 headless 会话”；退出中的会话
/// 即使仍出现在列表里也不能继续写入。
///
/// @author kongweiguang
fn agent_background_session_missing() -> CallToolResult {
    failure_with_data(
        AppError::conflict(
            "Terminal session was not found or is no longer running; create a headless session with terminal.create and pass its sessionId",
        ),
        json!({
            "setting": "terminal.agentCommandsInTerminal",
            "required": "explicitHeadlessSessionId",
            "route": "background",
            "fallback": "terminal.create"
        }),
    )
}

/// 提示必须结合本次解析出的目标，避免开启可见模式时引导助手写入未绑定或已失效的终端。
///
/// 未注入设置仓储时保持原有空 nextHints；读取失败时不推测模式。
///
/// @author kongweiguang
pub(crate) async fn agent_routing_hints(
    services: &McpServices,
    has_live_preferred_target: bool,
) -> Vec<String> {
    let ssh_available = ssh_command_available(services);
    match stored_agent_command_routing(services).await {
        Ok(None) => Vec::new(),
        Ok(Some(AgentCommandRouting::Visible)) if has_live_preferred_target => vec![
            "当前设置为在绑定的主终端 Tab 中执行：对首选终端使用 terminal.write 写入，并用 terminal.snapshot 读取输出；普通切换 Tab 不会重新绑定本会话。".to_owned(),
        ],
        Ok(Some(AgentCommandRouting::Visible)) => vec![
            "可见终端模式已开启，但当前没有可用的首选终端。请让用户确认目标，或在已确认的后台目标执行；不要自动选择其他标签页。".to_owned(),
        ],
        Ok(Some(AgentCommandRouting::Background)) => {
            let ssh_hint = if ssh_available {
                "，非交互远程命令也可使用 ssh.command"
            } else {
                ""
            };
            vec![format!(
                "当前设置为后台执行：不要写入首选可见终端；一次性或交互式命令都先用 terminal.create 创建 headless 会话并显式传入返回的 sessionId{ssh_hint}。"
            )]
        }
        Err(_) => vec![
            "无法读取 Agent 命令路由设置；写入可见终端前请先确认设置页中的终端命令路由状态。".to_owned(),
        ],
    }
}

/// 生成既兼容旧 MCP schema 又携带稳定 success/error 字段的结构化成功响应。
pub(crate) fn success(summary: String, data: Value, entities: Vec<Value>) -> CallToolResult {
    success_with_hints(summary, data, entities, Vec::new())
}

/// 在成功响应中填入实时路由提示，复用既有 nextHints 字段而不扩张 wire 外形。
///
/// @author kongweiguang
pub(crate) fn success_with_hints(
    summary: String,
    data: Value,
    entities: Vec<Value>,
    next_hints: Vec<String>,
) -> CallToolResult {
    CallToolResult::structured(json!({
        "summary": summary,
        "data": data,
        "entities": entities,
        "nextHints": next_hints
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
