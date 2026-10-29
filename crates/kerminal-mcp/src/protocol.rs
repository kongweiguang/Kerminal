//! RMCP handler、按场景收敛的工具目录与具名用例路由。
//!
//! @author kongweiguang

use std::sync::OnceLock;

use axum::http::request::Parts;
use rmcp::{
    ErrorData, RoleServer,
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTrackerToken};

use kerminal_domain::identity::AgentSessionId;

use crate::{
    McpServices, agent_tools, config_tools, files_tools, guides_tools, history_tools,
    port_forward_tools, ssh_tools, terminal_tools,
};

const TOOL_PAGE_SIZE: usize = 50;
const FROZEN_TOOL_CATALOG: &str = include_str!("../resources/tool_catalog.json");

/// 保留 legacy catalog 原始 wire 字段，避免 MCP Tool schema 与安全注解从 handler 推导。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolDefinition {
    name: String,
    title: String,
    description: String,
    category: String,
    annotations: CatalogAnnotations,
    enabled: bool,
    exposed_to_mcp: bool,
    agent_scoped: bool,
    input_schema: Map<String, Value>,
}

/// 把冻结注解字段独立解码，避免把 domain 或 handler 类型直接暴露成 MCP 协议对象。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogAnnotations {
    read_only_hint: bool,
    destructive_hint: bool,
    idempotent_hint: bool,
    open_world_hint: bool,
}

/// Fixture 元数据可附加来源与版本信息，工具定义仍按原始数组顺序作为分页顺序。
#[derive(Debug, Deserialize)]
struct CatalogFixture {
    tools: Vec<ToolDefinition>,
}

/// 缓存固定核心目录，仅按当前 endpoint 的 Agent 上下文增加会话入口。
fn frozen_catalog() -> &'static CatalogFixture {
    static CATALOG: OnceLock<CatalogFixture> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(FROZEN_TOOL_CATALOG)
            .expect("frozen MCP catalog fixture must remain valid JSON")
    })
}

/// 目录与调用门禁共用选择规则，避免隐藏工具仍可通过猜测名称执行。
fn tool_definitions(services: &McpServices) -> Vec<Tool> {
    frozen_catalog()
        .tools
        .iter()
        .filter(|definition| tool_is_visible(definition, services))
        .map(|definition| {
            let _category = &definition.category;
            Tool::new(
                definition.name.clone(),
                definition.description.clone(),
                definition.input_schema.clone(),
            )
            .with_title(definition.title.clone())
            .with_annotations(ToolAnnotations::from_raw(
                None,
                Some(definition.annotations.read_only_hint),
                Some(definition.annotations.destructive_hint),
                Some(definition.annotations.idempotent_hint),
                Some(definition.annotations.open_world_hint),
            ))
        })
        .collect()
}

/// 帮助仅描述当前 endpoint 可调用的工具，元数据与 tools/list 复用同一选择规则。
pub(crate) fn tool_catalog_entries(services: &McpServices) -> Vec<Value> {
    frozen_catalog()
        .tools
        .iter()
        .filter(|definition| tool_is_visible(definition, services))
        .map(|definition| {
            json!({
                "toolId": definition.name.as_str(),
                "title": definition.title.as_str(),
                "description": definition.description.as_str(),
                "category": definition.category.as_str(),
                "agentScoped": definition.agent_scoped,
                "family": tool_family(&definition.name),
                "inputSchema": &definition.input_schema,
                "annotations": {
                    "readOnlyHint": definition.annotations.read_only_hint,
                    "destructiveHint": definition.annotations.destructive_hint,
                    "idempotentHint": definition.annotations.idempotent_hint,
                    "openWorldHint": definition.annotations.open_world_hint
                }
            })
        })
        .collect()
}

/// 以公开前缀归并工具族，令 help 和 guide 对外呈现同一套可发现名称。
pub(crate) fn tool_family(tool_id: &str) -> &'static str {
    match tool_id {
        "kerminal.help" | "kerminal.runtime_snapshot" => "discovery",
        "kerminal.agent.target_context" => "agent-session",
        "kerminal.config.validate" => "config",
        "kerminal.host.upsert_with_credential" | "kerminal.vault.encrypt_secret" => "credentials",
        "ssh.command" => "ssh",
        "sftp.transfer.enqueue"
        | "sftp.transfer.list"
        | "sftp.transfer.cancel"
        | "sftp.transfer.retry" => "sftp",
        "port_forward.create" | "port_forward.list" | "port_forward.close" => "port-forward",
        "history.search" => "history",
        _ => "terminal",
    }
}

/// RMCP protocol adapter；业务调用仅经注入的 application service。
#[derive(Clone)]
pub(crate) struct KerminalMcpHandler {
    services: McpServices,
    _service_lifetime: TaskTrackerToken,
}

impl KerminalMcpHandler {
    /// 为 RMCP session worker 绑定跟踪令牌，使桌面关停可以等到服务对象实际释放。
    pub(crate) fn new(services: McpServices, service_lifetime: TaskTrackerToken) -> Self {
        Self {
            services,
            _service_lifetime: service_lifetime,
        }
    }
}

impl ServerHandler for KerminalMcpHandler {
    /// 仅暴露运行态工具能力；没有 resources、prompts 或宿主审批能力。
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kerminal", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Kerminal MCP 只操作运行态能力；配置文件 CRUD 与调用审批由外部 host 负责。",
            )
    }

    /// 路由工具并让应用 executor 监督完整 handler；请求取消先传递给用例清理再返回。
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_id = request.name.to_string();
        let services = self.scoped_services(&context);
        if !is_catalog_tool(&tool_id, &services) {
            return Err(unknown_tool(&tool_id));
        }
        let arguments = request.arguments.unwrap_or_default();
        let request_cancellation = context.ct.clone();
        let request_services = services.clone();
        let output = services
            .run_request(request_cancellation, move |cancellation| async move {
                Ok(dispatch_tool(&tool_id, &request_services, arguments, cancellation).await)
            })
            .await
            .unwrap_or_else(terminal_tools::failure);
        Ok(output)
    }

    /// 按 endpoint 的真实暴露集合分页，Agent 上下文工具只在会话 endpoint 出现。
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let offset = request
            .as_ref()
            .and_then(|params| params.cursor.as_deref())
            .map(str::parse::<usize>)
            .transpose()
            .map_err(|_| invalid_cursor("Invalid tool pagination cursor"))?
            .unwrap_or(0);
        let all_tools = tool_definitions(&self.scoped_services(&context));
        if offset > all_tools.len() {
            return Err(invalid_cursor("Tool pagination cursor is out of range"));
        }
        let end = offset.saturating_add(TOOL_PAGE_SIZE).min(all_tools.len());
        Ok(ListToolsResult {
            meta: None,
            next_cursor: (end < all_tools.len()).then(|| end.to_string()),
            tools: all_tools
                .into_iter()
                .skip(offset)
                .take(end - offset)
                .collect(),
        })
    }
}

/// 把请求范围里的 Agent 身份变成不可变 service facade，避免 handler 间共享 scope。
impl KerminalMcpHandler {
    /// 为单次请求克隆并绑定 Agent scope，避免把请求身份写回共享服务或跨请求泄漏。
    fn scoped_services(&self, context: &RequestContext<RoleServer>) -> McpServices {
        context
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<AgentSessionId>())
            .cloned()
            .map(|agent_session_id| self.services.clone().for_agent_session(agent_session_id))
            .unwrap_or_else(|| self.services.clone())
    }
}

/// 显式注册工具 ID 到业务族 adapter，未知名称返回 None 而不猜测近似路由。
async fn dispatch_tool(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    match tool_id {
        "kerminal.help" | "kerminal.runtime_snapshot" => {
            guides_tools::dispatch(tool_id, services, arguments, cancellation).await
        }
        "kerminal.config.validate" => {
            config_tools::validate(services, arguments, cancellation).await
        }
        "terminal.create" => terminal_tools::create(services, arguments, cancellation).await,
        "terminal.write" => terminal_tools::write(services, arguments, cancellation).await,
        "terminal.snapshot" => terminal_tools::snapshot(services, arguments, cancellation).await,
        "kerminal.agent.target_context" => {
            agent_tools::target_context(services, arguments, cancellation).await
        }
        "terminal.resize" => terminal_tools::resize(services, arguments, cancellation).await,
        "terminal.list" => terminal_tools::list(services, arguments, cancellation).await,
        "terminal.close" => terminal_tools::close(services, arguments, cancellation).await,
        "kerminal.host.upsert_with_credential" | "kerminal.vault.encrypt_secret" => {
            crate::credentials_tools::dispatch(tool_id, services, arguments, cancellation).await
        }
        "ssh.command" => ssh_tools::dispatch(tool_id, services, arguments, cancellation).await,
        "sftp.transfer.enqueue"
        | "sftp.transfer.list"
        | "sftp.transfer.cancel"
        | "sftp.transfer.retry" => {
            files_tools::dispatch(tool_id, services, arguments, cancellation).await
        }
        "port_forward.create" | "port_forward.list" | "port_forward.close" => {
            port_forward_tools::dispatch(tool_id, services, arguments, cancellation).await
        }
        "history.search" => {
            history_tools::dispatch(tool_id, services, arguments, cancellation).await
        }
        _ => terminal_tools::failure(kerminal_application::AppError::unsupported(
            "MCP tool handler is not connected",
        )),
    }
}

/// 固定目录只区分全局核心与会话上下文，不再保留可重新启用的工具组。
fn tool_is_visible(definition: &ToolDefinition, services: &McpServices) -> bool {
    definition.enabled
        && definition.exposed_to_mcp
        && (!definition.agent_scoped || services.agent_scope.is_some())
}

/// 调用与发现共用固定目录；已删除工具及全局 endpoint 的会话入口均不能猜名调用。
fn is_catalog_tool(tool_id: &str, services: &McpServices) -> bool {
    frozen_catalog()
        .tools
        .iter()
        .any(|definition| definition.name == tool_id && tool_is_visible(definition, services))
}

/// 未知 MCP method 使用协议级 METHOD_NOT_FOUND，不伪装成业务执行错误。
fn unknown_tool(tool_id: &str) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode::METHOD_NOT_FOUND,
        "Unknown Kerminal tool",
        Some(json!({ "toolId": tool_id })),
    )
}

/// 将不合法的 cursor 映射为一致的 JSON-RPC invalid params 错误。
fn invalid_cursor(message: &'static str) -> ErrorData {
    ErrorData::new(rmcp::model::ErrorCode::INVALID_PARAMS, message, None)
}
