//! 服务器资源快照的 MCP 参数与结果映射。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError,
    operations::{ServerInfoQuery, ServerInfoTier},
};
use kerminal_domain::{identity::HostId, operations::ServerInfoTarget};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotArgs {
    host_id: String,
}

/// 明确按 hostId 查询完整快照，不从 UI 选中项或当前终端推导监控目标。
pub(crate) async fn snapshot(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("服务器信息查询已取消"));
        }
        let args: SnapshotArgs = parse_args(arguments)?;
        let host_id = HostId::parse(&args.host_id)
            .map_err(|_| AppError::invalid("服务器信息 hostId 无效"))?;
        let snapshot = services
            .require_operations()?
            .server_info
            .snapshot(
                ServerInfoQuery {
                    target: ServerInfoTarget::Ssh { host_id },
                    tier: ServerInfoTier::Full,
                },
                cancellation,
            )
            .await?;
        serde_json::to_value(snapshot).map_err(|_| AppError::infrastructure("服务器信息无法编码"))
    }
    .await;

    match result {
        Ok(data) => success("服务器信息快照已获取".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}
