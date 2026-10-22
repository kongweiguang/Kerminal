//! 容器、tmux、服务器信息和端口转发的 MCP 运行态适配。
//!
//! @author kongweiguang

mod containers;
mod port_forward;
mod server_info;
mod tmux;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::{McpServices, terminal_tools::failure};

/// 按冻结工具 ID 显式分派到应用用例，MCP 层不访问基础设施或创建第二套队列。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> rmcp::model::CallToolResult {
    match tool_id {
        "server_info.snapshot" => server_info::snapshot(services, arguments, cancellation).await,
        "port_forward.create" | "port_forward.list" | "port_forward.close" => {
            port_forward::dispatch(tool_id, services, arguments, cancellation).await
        }
        "container.list"
        | "container.inspect"
        | "container.logs.tail"
        | "container.stats"
        | "container.start"
        | "container.stop"
        | "container.restart"
        | "container.remove"
        | "container.files.list"
        | "container.files.preview"
        | "container.files.write_text"
        | "container.files.create_directory"
        | "container.files.rename"
        | "container.files.chmod"
        | "container.files.upload"
        | "container.files.download"
        | "container.files.delete" => {
            containers::dispatch(tool_id, services, arguments, cancellation).await
        }
        "tmux.probe"
        | "tmux.list_sessions"
        | "tmux.create_session"
        | "tmux.rename_session"
        | "tmux.kill_session"
        | "tmux.list_windows"
        | "tmux.list_panes"
        | "tmux.capture_pane"
        | "tmux.attach_plan" => tmux::dispatch(tool_id, services, arguments, cancellation).await,
        _ => failure(kerminal_application::AppError::unsupported(
            "未注册的运维工具",
        )),
    }
}
