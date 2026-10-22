//! MCP 运行诊断工具到 application diagnostics 用例的适配。
//!
//! @author kongweiguang

use kerminal_application::AppError;
use rmcp::model::CallToolResult;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, success},
};

/// 把健康探测和本地脱敏包生成交给 diagnostics application service，handler 不读原始数据源。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if !arguments.is_empty() {
        return failure(AppError::invalid("tool arguments"));
    }
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("Diagnostics request was cancelled"));
    }
    let service = match services.require_diagnostics() {
        Ok(service) => service,
        Err(error) => return failure(error),
    };
    match tool_id {
        "diagnostics.runtime_health" => match service.runtime_health().await {
            Ok(snapshot) => {
                let summary = format!(
                    "运行体检已读取：{} {}，采样来源 {}，结果已脱敏。",
                    snapshot.system.os, snapshot.system.arch, snapshot.sampling.source,
                );
                match serde_json::to_value(&snapshot) {
                    Ok(snapshot) => {
                        success(summary, json!({ "runtimeHealth": snapshot }), Vec::new())
                    }
                    Err(error) => failure(AppError::infrastructure_with_source(
                        "Runtime health serialization failed",
                        error,
                    )),
                }
            }
            Err(error) => failure(error),
        },
        "diagnostics.create_bundle" => match service.create_bundle().await {
            Ok(bundle) => {
                let redacted = if bundle.redacted { "是" } else { "否" };
                let summary = format!(
                    "诊断包已生成：{}，写入 {} 字节，分区 {} 个，已脱敏：{}，路径：{}。",
                    bundle.file_name,
                    bundle.bytes_written,
                    bundle.sections.len(),
                    redacted,
                    bundle.relative_path,
                );
                success(
                    summary,
                    json!({
                        "bundle": {
                            "id": bundle.id,
                            "createdAt": bundle.created_at,
                            "fileName": bundle.file_name,
                            // 旧协议字段 path 保留，但只暴露应用固定的相对路径。
                            "path": bundle.relative_path,
                            "bytesWritten": bundle.bytes_written,
                            "sections": bundle.sections,
                            "redacted": bundle.redacted
                        }
                    }),
                    vec![json!({ "type": "diagnosticBundle", "id": bundle.id })],
                )
            }
            Err(error) => failure(error),
        },
        _ => failure(AppError::invalid("Unknown diagnostics tool")),
    }
}
