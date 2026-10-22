//! MCP 命令历史的只读查询适配。
//!
//! @author kongweiguang

use kerminal_application::AppError;
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandHistorySource, CommandHistoryTarget,
    },
    identity::{HostId, PaneId, SessionId},
};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HistorySearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    remote_host_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// 仅派发冻结的 history.search；MCP 不获得历史写入、配置或目录管理入口。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if tool_id != "history.search" {
        return failure(AppError::unsupported("history tool is not available"));
    }
    history_search(services, arguments, cancellation).await
}

/// 查询前后都观察 host 传入的取消令牌，确保迟到结果不会继续返回给已取消调用。
pub(crate) async fn history_search(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("history search cancelled"));
    }
    let args = match parse_args::<HistorySearchArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let request = match build_history_request(args) {
        Ok(request) => request,
        Err(error) => return failure(error),
    };
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("history search cancelled"));
    }
    let commands = match services.require_commands() {
        Ok(commands) => commands,
        Err(error) => return failure(error),
    };
    let entries = match commands.list_history(request).await {
        Ok(entries) => entries,
        Err(error) => return failure(error),
    };
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("history search cancelled"));
    }
    success(summarize_history(&entries), json!({}), Vec::new())
}

/// 把 MCP 边界字符串解析为领域值类型，并拒绝历史工具契约未公开的过滤枚举。
fn build_history_request(args: HistorySearchArgs) -> Result<CommandHistoryListRequest, AppError> {
    let target = match args.target.as_deref() {
        None => None,
        Some("local") => Some(CommandHistoryTarget::Local),
        Some("ssh") => Some(CommandHistoryTarget::Ssh),
        Some(_) => return Err(AppError::invalid("target must be local or ssh")),
    };
    let source = match args.source.as_deref() {
        None => None,
        Some("user") => Some(CommandHistorySource::User),
        Some("snippet") => Some(CommandHistorySource::Snippet),
        Some("broadcast") => Some(CommandHistorySource::Broadcast),
        Some("tool") => Some(CommandHistorySource::Tool),
        Some(_) => {
            return Err(AppError::invalid(
                "source is not supported by history.search",
            ));
        }
    };
    Ok(CommandHistoryListRequest {
        query: args.query,
        source,
        target,
        pane_id: args
            .pane_id
            .map(|value| PaneId::parse(&value).map_err(|_| AppError::invalid("paneId")))
            .transpose()?,
        remote_host_id: args
            .remote_host_id
            .map(|value| HostId::parse(&value).map_err(|_| AppError::invalid("remoteHostId")))
            .transpose()?,
        session_id: args
            .session_id
            .map(|value| SessionId::parse(&value).map_err(|_| AppError::invalid("sessionId")))
            .transpose()?,
        limit: args.limit,
    })
}

/// 保持旧工具的无命中提示和最近五条摘要；结构化结果仍为空对象以兼容冻结外部响应。
fn summarize_history(entries: &[CommandHistoryEntry]) -> String {
    if entries.is_empty() {
        return "没有匹配的命令历史。".to_owned();
    }
    let samples = entries
        .iter()
        .take(5)
        .map(|entry| {
            let target = match entry.target {
                CommandHistoryTarget::Local => "本地",
                CommandHistoryTarget::Ssh => "SSH",
                CommandHistoryTarget::Telnet => "Telnet",
                CommandHistoryTarget::Serial => "Serial",
                CommandHistoryTarget::Container => "容器",
            };
            format!(
                "{target}: {}",
                truncate_summary(&collapse_whitespace(&entry.command))
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    format!("找到 {} 条命令历史。最近记录：{samples}。", entries.len())
}

/// 将多行命令压成单行摘要，不改变返回给旧 MCP consumer 的历史正文。
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 以 Unicode 字符边界截断摘要，避免超长命令污染 Agent 上下文或破坏 UTF-8。
fn truncate_summary(value: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut characters = value.chars();
    let truncated = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
#[path = "../tests/unit/history_tools.rs"]
mod tests;
