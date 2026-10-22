//! tmux MCP 参数适配，所有操作都通过 application 的固定 tmux 用例执行。
//!
//! @author kongweiguang

use std::time::{SystemTime, UNIX_EPOCH};

use kerminal_application::{AppError, operations::TmuxCreateRequest};
use kerminal_domain::{
    identity::HostId,
    operations::{
        TmuxAttachPlan, TmuxMutationReceipt, TmuxPane, TmuxSession, TmuxTarget, TmuxWindow,
    },
};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TmuxArgs {
    target_kind: Option<String>,
    host_id: Option<String>,
    profile_id: Option<String>,
    socket_name: Option<String>,
    socket_path: Option<String>,
    tmux_path: Option<String>,
    name: Option<String>,
    cwd: Option<String>,
    session_id: Option<String>,
    session_name: Option<String>,
    target_id: Option<String>,
    pane_id: Option<String>,
    lines: Option<u32>,
}

/// 将 tmux 工具 family 映射到单一 session/command registry 支持的只读及写入用例。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    match tool_id {
        "tmux.probe" => probe(services, arguments, cancellation).await,
        "tmux.list_sessions" => list_sessions(services, arguments, cancellation).await,
        "tmux.create_session" => create_session(services, arguments, cancellation).await,
        "tmux.rename_session" => rename_session(services, arguments, cancellation).await,
        "tmux.kill_session" => kill_session(services, arguments, cancellation).await,
        "tmux.list_windows" => list_windows(services, arguments, cancellation).await,
        "tmux.list_panes" => list_panes(services, arguments, cancellation).await,
        "tmux.capture_pane" => capture_pane(services, arguments, cancellation).await,
        "tmux.attach_plan" => attach_plan(services, arguments, cancellation).await,
        _ => failure(AppError::unsupported("未注册的 tmux 工具")),
    }
}

/// 检测所选本机或 SSH tmux，不把未安装能力折叠成连接异常。
async fn probe(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result: Result<Value, AppError> = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let target = target(&args)?;
        let target_ref = legacy_target_ref(&target);
        let capability = services
            .require_operations()?
            .tmux
            .probe(target, cancellation)
            .await?;
        Ok(json!({
            "status": {
                "targetRef": target_ref,
                "target": legacy_target_value(&args),
                "available": capability.available,
                "version": capability.version,
                "reason": capability.reason,
                "socketName": args.socket_name,
                "socketPath": args.socket_path
            }
        }))
    }
    .await;
    match result {
        Ok(data) => success("tmux 能力已探测".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 查询 server session 列表，socket 选项与 host 共同构成稳定 target scope。
async fn list_sessions(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result: Result<Vec<Value>, AppError> = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let selected_target = target(&args)?;
        let target_ref = legacy_target_ref(&selected_target);
        let sessions = services
            .require_operations()?
            .tmux
            .list_sessions(selected_target, cancellation)
            .await?;
        Ok(sessions
            .into_iter()
            .map(|session| legacy_session_value(session, &target_ref))
            .collect::<Vec<_>>())
    }
    .await;
    match result {
        Ok(sessions) => success(
            format!("查询到 {} 个 tmux session", sessions.len()),
            legacy_collection("sessions", sessions.clone()),
            sessions,
        ),
        Err(error) => failure(error),
    }
}

/// 创建 detached tmux session，并保留创建成功但摘要刷新可重试的应用回执。
async fn create_session(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let selected_target = target(&args)?;
        let target_ref = legacy_target_ref(&selected_target);
        let receipt = services
            .require_operations()?
            .tmux
            .create_session(
                TmuxCreateRequest {
                    target: selected_target,
                    name: required(&args.name, "name")?.to_owned(),
                    working_directory: args.cwd,
                },
                cancellation,
            )
            .await?;
        Ok(legacy_mutation_value(receipt, &target_ref))
    }
    .await;
    match result {
        Ok(data) => success("tmux session 已创建".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 重命名 session 后把应用层独立刷新标志直接返回给调用方。
async fn rename_session(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let selected_target = target(&args)?;
        let target_ref = legacy_target_ref(&selected_target);
        let receipt = services
            .require_operations()?
            .tmux
            .rename_session(
                selected_target,
                required(&args.session_id, "sessionId")?.to_owned(),
                required(&args.name, "name")?.to_owned(),
                cancellation,
            )
            .await?;
        Ok(legacy_mutation_value(receipt, &target_ref))
    }
    .await;
    match result {
        Ok(data) => success("tmux session 已重命名".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 删除指定 session；已不存在按 application 的幂等规则处理。
async fn kill_session(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let session_id = required(&args.session_id, "sessionId")?.to_owned();
        let killed = services
            .require_operations()?
            .tmux
            .kill_session(target(&args)?, session_id.clone(), cancellation)
            .await?;
        Ok((legacy_kill_value(&session_id, killed), session_id, killed))
    }
    .await;
    match result {
        Ok((data, session_id, true)) => success(
            format!("tmux session 已结束：{session_id}。"),
            data.clone(),
            vec![data],
        ),
        Ok((data, session_id, false)) => success(
            format!("tmux session 不存在或已结束：{session_id}。"),
            data.clone(),
            vec![data],
        ),
        Err(error) => failure(error),
    }
}

/// 列出指定 session 的窗口详情。
async fn list_windows(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let session_id = required(&args.session_id, "sessionId")?.to_owned();
        let selected_target = target(&args)?;
        let target_ref = legacy_target_ref(&selected_target);
        let items = services
            .require_operations()?
            .tmux
            .list_windows(selected_target, session_id.clone(), cancellation)
            .await?;
        items
            .into_iter()
            .map(|window| legacy_window_value(window, &session_id, &target_ref))
            .collect::<Result<Vec<_>, _>>()
    }
    .await;
    match result {
        Ok(items) => success(
            format!("查询到 {} 个 tmux window", items.len()),
            legacy_collection("windows", items.clone()),
            items,
        ),
        Err(error) => failure(error),
    }
}

/// 列出 session、window 或 pane 下的 pane 摘要。
async fn list_panes(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let selected_target = target(&args)?;
        let target_ref = legacy_target_ref(&selected_target);
        let items = services
            .require_operations()?
            .tmux
            .list_panes(
                selected_target,
                required(&args.target_id, "targetId")?.to_owned(),
                cancellation,
            )
            .await?;
        items
            .into_iter()
            .map(|pane| legacy_pane_value(pane, &target_ref))
            .collect::<Result<Vec<_>, _>>()
    }
    .await;
    match result {
        Ok(items) => success(
            format!("查询到 {} 个 tmux pane", items.len()),
            legacy_collection("panes", items.clone()),
            items,
        ),
        Err(error) => failure(error),
    }
}

/// 捕获限定行数的 pane 文本，响应保留真实截断信息。
async fn capture_pane(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: TmuxArgs = parse_args(arguments)?;
        let lines = args.lines.unwrap_or(200);
        let capture = services
            .require_operations()?
            .tmux
            .capture_pane(
                target(&args)?,
                required(&args.pane_id, "paneId")?.to_owned(),
                lines,
                cancellation,
            )
            .await?;
        Ok(legacy_capture_value(capture, lines))
    }
    .await;
    match result {
        Ok(data) => success("tmux pane 快照已读取".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 仅生成显式 attach 命令规格，MCP 读取计划不会隐式创建终端 session。
async fn attach_plan(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: TmuxArgs = parse_args(arguments)?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("tmux attach 计划查询已取消"));
        }
        let plan = services
            .require_operations()?
            .tmux
            .attach_plan(
                target(&args)?,
                required(&args.session_id, "sessionId")?.to_owned(),
                args.session_name,
                args.cwd,
                cancellation,
            )
            .await?;
        Ok(legacy_attach_value(plan))
    }
    .await;
    match result {
        Ok(data) => success(
            "tmux attach 计划已生成".to_owned(),
            data.clone(),
            vec![data],
        ),
        Err(error) => failure(error),
    }
}

/// 将旧 targetKind/hostId wire 组合规范化为内部 host 与 socket 作用域。
fn target(args: &TmuxArgs) -> Result<TmuxTarget, AppError> {
    let kind = required(&args.target_kind, "targetKind")?;
    let host_id = match kind {
        "local" if args.host_id.is_none() => None,
        "local" => return Err(AppError::invalid("targetKind=local 不应提供 hostId")),
        "ssh" => Some(
            HostId::parse(required(&args.host_id, "hostId")?)
                .map_err(|_| AppError::invalid("tmux hostId 无效"))?,
        ),
        _ => return Err(AppError::invalid("targetKind 只支持 local 或 ssh")),
    };
    Ok(TmuxTarget {
        host_id,
        profile_id: args.profile_id.clone(),
        socket_name: args.socket_name.clone(),
        socket_path: args.socket_path.clone(),
        tmux_path: args.tmux_path.clone(),
    })
}

/// 从可选字符串中取出非空必填值，不回显用户输入到错误消息。
fn required<'a>(value: &'a Option<String>, field: &'static str) -> Result<&'a str, AppError> {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::invalid(field))
}

/// 把固定基线的 MCP data key 保留为 family 名称，entities 仍单独承载可检索列表。
fn legacy_collection(key: &str, values: Vec<Value>) -> Value {
    let mut object = Map::new();
    object.insert(key.to_owned(), Value::Array(values));
    Value::Object(object)
}

/// 保留旧 kill 回执的 bool 语义，使调用方能区分确实删除与目标已不存在。
fn legacy_kill_value(session_id: &str, killed: bool) -> Value {
    json!({ "sessionId": session_id, "killed": killed })
}

/// 将新 window 投影补回旧 wrapper/key，同时保留 application 的新增字段。
fn legacy_window_value(
    window: TmuxWindow,
    session_id: &str,
    target_ref: &str,
) -> Result<Value, AppError> {
    let mut value = to_value(window)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| AppError::infrastructure("tmux window 结果不是对象"))?;
    let pane_count = object
        .get("paneCount")
        .cloned()
        .ok_or_else(|| AppError::infrastructure("tmux window 结果缺少 paneCount"))?;
    object.insert("sessionId".to_owned(), json!(session_id));
    object.insert("panes".to_owned(), pane_count);
    object.insert("targetRef".to_owned(), json!(target_ref));
    Ok(value)
}

/// 将新 pane 结果适配旧的 nullable 文本字段，不把缺失内容伪装成空命令或标题。
fn legacy_pane_value(pane: TmuxPane, target_ref: &str) -> Result<Value, AppError> {
    let mut value = to_value(pane)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| AppError::infrastructure("tmux pane 结果不是对象"))?;
    for field in ["title", "currentCommand"] {
        if object.get(field).and_then(Value::as_str) == Some("") {
            object.insert(field.to_owned(), Value::Null);
        }
    }
    object.insert("targetRef".to_owned(), json!(target_ref));
    Ok(value)
}

/// 序列化 application DTO 并把编码失败作为错误返回，不静默丢弃结果项。
fn to_value(value: impl serde::Serialize) -> Result<Value, AppError> {
    serde_json::to_value(value).map_err(|_| AppError::infrastructure("tmux 结果无法编码"))
}

/// 用固定版本的字段名称输出当前已确认存在的 session，不伪造历史 stale 状态。
fn legacy_session_value(session: TmuxSession, target_ref: &str) -> Value {
    json!({
        "id": session.id,
        "name": session.name,
        "attached": session.attached,
        "windows": session.windows,
        "clients": session.clients,
        "currentPath": session.current_path,
        "createdAt": session.created_at,
        "activityAt": session.activity_at,
        "targetRef": target_ref,
        "status": "running"
    })
}

/// 保留 hostId 与 profileId 的旧 tagged target wire 形状，避免暴露连接细节。
fn legacy_target_value(args: &TmuxArgs) -> Value {
    match args.target_kind.as_deref() {
        Some("ssh") => json!({"kind": "ssh", "hostId": args.host_id}),
        _ => json!({"kind": "local", "profileId": args.profile_id}),
    }
}

/// 按 v0.3.45 算法重建 socket-scoped targetRef，使同一主机上的不同 tmux server 不冲突。
fn legacy_target_ref(target: &TmuxTarget) -> String {
    let mut value = target.host_id.as_ref().map_or_else(
        || {
            target
                .profile_id
                .as_deref()
                .map_or_else(|| "local".to_owned(), |id| format!("local:{id}"))
        },
        |host_id| format!("ssh:{}", host_id.as_str()),
    );
    if let Some(socket) = target
        .socket_name
        .as_deref()
        .filter(|socket| !socket.trim().is_empty())
    {
        value.push_str("|L:");
        value.push_str(socket);
    }
    if let Some(socket) = target
        .socket_path
        .as_deref()
        .filter(|socket| !socket.trim().is_empty())
    {
        value.push_str("|S:");
        value.push_str(socket);
    }
    value
}

/// 保留旧的 session wrapper 与 status，同时附带新用例的 refreshRequired 事实。
fn legacy_mutation_value(receipt: TmuxMutationReceipt, target_ref: &str) -> Value {
    json!({
        "session": legacy_session_value(receipt.session, target_ref),
        "refreshRequired": receipt.refresh_required
    })
}

/// 兼容旧 capture 中 `lines` 表示请求上限的语义，并单独报告实际捕获行数。
fn legacy_capture_value(
    capture: kerminal_domain::operations::TmuxPaneCapture,
    lines: u32,
) -> Value {
    json!({
        "capture": {
            "paneId": capture.pane_id,
            "text": capture.text,
            "lines": lines,
            "truncated": capture.truncated,
            "capturedLines": capture.captured_lines
        }
    })
}

/// 将 attach plan 转换成旧 tagged union；local 与 SSH 各自保持原执行字段语义。
fn legacy_attach_value(plan: TmuxAttachPlan) -> Value {
    let title = format!("tmux: {}", plan.session_name);
    let binding = legacy_binding_value(&plan.target, &plan.session_id, &plan.session_name);
    if let Some(host_id) = plan.target.host_id.as_ref() {
        let remote_command = std::iter::once(plan.program.as_str())
            .chain(plan.arguments.iter().map(String::as_str))
            .map(shell_quote_posix)
            .collect::<Vec<_>>()
            .join(" ");
        json!({
            "launch": {
                "mode": "ssh",
                "hostId": host_id.as_str(),
                "remoteCommand": remote_command,
                "cwd": plan.working_directory,
                "title": title,
                "binding": binding
            }
        })
    } else {
        json!({
            "launch": {
                "mode": "local",
                "terminal": {
                    "shell": plan.program,
                    "args": plan.arguments,
                    "cwd": plan.working_directory,
                    "env": {},
                    "cols": 80,
                    "rows": 24,
                    "cleanupPaths": []
                },
                "title": title,
                "binding": binding
            }
        })
    }
}

/// 以 shell 单引号规则转义旧 SSH attach plan 中的 argv 元素，不执行该字符串。
fn shell_quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 按旧字段保留目标、socket 与计划生成时间，时间只标记绑定创建而非已登录。
fn legacy_binding_value(target: &TmuxTarget, session_id: &str, session_name: &str) -> Value {
    let attached_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
        .to_string();
    json!({
        "targetRef": legacy_target_ref(target),
        "sessionId": session_id,
        "sessionName": session_name,
        "socketName": target.socket_name,
        "socketPath": target.socket_path,
        "attachedAt": attached_at
    })
}

#[cfg(test)]
#[path = "../../tests/unit/tmux_tools.rs"]
mod tests;
