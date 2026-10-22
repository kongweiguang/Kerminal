//! MCP 对文件化配置 validator 的窄适配。
//!
//! @author kongweiguang

use kerminal_application::{AppError, preferences::ConfigurationValidationScope};
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
struct ValidateArgs {
    #[serde(default)]
    scope: Option<String>,
}

/// 通过应用层只读 validator 校验真实配置，并将无效报告作为正常业务结果返回。
pub(crate) async fn validate(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled(
            "Configuration validation was cancelled",
        ));
    }
    let args = match parse_args::<ValidateArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let scope = match ConfigurationValidationScope::parse(args.scope.as_deref()) {
        Ok(scope) => scope,
        Err(error) => return failure(error),
    };
    let service = match services.require_configuration_validation() {
        Ok(service) => service,
        Err(error) => return failure(error),
    };
    let report = match service.validate(scope).await {
        Ok(report) => report,
        Err(error) => return failure(error),
    };
    let result = match serde_json::to_value(&report) {
        Ok(result) => result,
        Err(error) => {
            return failure(AppError::infrastructure_with_source(
                "Configuration validation report serialization failed",
                error,
            ));
        }
    };
    success(
        format!(
            "配置校验完成：scope={}，checked={}，errors={}，warnings={}。",
            report.scope.as_str(),
            report.checked.len(),
            report.error_count,
            report.warning_count,
        ),
        json!({ "report": result }),
        Vec::new(),
    )
}
