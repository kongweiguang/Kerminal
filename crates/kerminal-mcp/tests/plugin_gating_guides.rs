//! 验证帮助与运行态快照按插件启用视图裁剪推荐内容。
//!
//! 帮助工具容易被外部 Agent 当作调用入口，因此这里走真实 RMCP 往返，
//! 同时核对目录、工作流、快照工具族与 refreshTools，而不是只测内部映射。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    plugins::{PluginId, PluginRegistry, PluginRegistryBuilder},
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionFilter, SessionRuntimeMetadata,
        SessionService, SessionSubscription, ShellCommandOutcome, ShellCommandRequest,
        TerminalCommandContext, TerminalInputEvent, TerminalPalette, TerminalSearchRequest,
        TerminalSearchResult, TerminalSelection, UiCommandLineRequest, UiSessionBinding,
    },
};
use kerminal_domain::{
    identity::{AgentSessionId, SessionId},
    sessions::SessionInfo,
};
use kerminal_mcp::{McpServer, McpServerConfig, McpServerHandle, McpServices};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ClientInfo, InitializeRequestParams},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Map, Value, json};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

/// 帮助工具不需要真实 PTY；空会话列表足以驱动 runtime_snapshot，其余端口显式拒绝。
#[derive(Default)]
struct EmptySessionService;

#[async_trait]
impl SessionService for EmptySessionService {
    /// 帮助测试不创建真实会话，显式拒绝避免隐藏 fake 能力。
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        Err(AppError::unsupported("test session create"))
    }

    /// 取消路径与 create 同样拒绝，保持 fake 的边界可见。
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test session create cancelled"));
        }
        Err(AppError::unsupported("test session create"))
    }

    /// 快照只读取空列表，确保运行态数据来自实际 application service。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        Ok(Vec::new())
    }

    /// 测试 fake 没有真实 transport 元数据。
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        Err(AppError::unsupported("test runtime metadata"))
    }

    /// 测试 fake 不模拟输入字节流。
    async fn write(&self, _session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        Err(AppError::unsupported("test terminal write"))
    }

    /// 测试 fake 不模拟 GPUI 键盘编码。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test terminal input event"))
    }

    /// 测试 fake 不实现可见 UI 的命令行提交。
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        Err(AppError::unsupported("test UI command line"))
    }

    /// 测试 fake 不调整真实网格。
    async fn resize(
        &self,
        _session_id: SessionId,
        _size: kerminal_domain::sessions::TerminalSize,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test terminal resize"))
    }

    /// 测试 fake 不维护主题 palette。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test palette"))
    }

    /// 测试 fake 不裁剪 scrollback。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        Err(AppError::unsupported("test scrollback limit"))
    }

    /// 测试 fake 不生成 GPUI 渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        Err(AppError::unsupported("test render snapshot"))
    }

    /// 测试 fake 不生成纯文本快照。
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        Err(AppError::unsupported("test plaintext snapshot"))
    }

    /// 测试 fake 不维护滚动视口。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        Err(AppError::unsupported("test scroll"))
    }

    /// 测试 fake 不维护 UI 绑定滚动。
    async fn scroll_ui(
        &self,
        _binding: UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test UI scroll"))
    }

    /// 测试 fake 不维护网格选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test selection"))
    }

    /// 测试 fake 没有选中文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        Ok(None)
    }

    /// 测试 fake 不实现 scrollback 搜索。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        Err(AppError::unsupported("test search"))
    }

    /// 测试 fake 没有可重连目标。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test reconnect cancelled"));
        }
        Err(AppError::unsupported("test reconnect"))
    }

    /// 测试 fake 没有可关闭会话。
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        Err(AppError::not_found("session not found"))
    }

    /// 测试 fake 立即完成关闭，不维护独立 deadline。
    async fn close_until(
        &self,
        session_id: SessionId,
        _deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        self.close(session_id).await
    }

    /// 测试 fake 没有可清理的会话。
    async fn close_all(&self) -> AppResult<()> {
        Ok(())
    }

    /// 测试 fake 不广播 UI 更新。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        Err(AppError::unsupported("test subscribe"))
    }

    /// 测试 fake 不执行交互 shell 命令。
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test shell command cancelled"));
        }
        Err(AppError::unsupported("test shell command"))
    }
}

/// 构造带指定禁用单元的真实冻结视图；其余内置单元保持启用。
///
/// @author kongweiguang
fn registry_with_disabled(disabled: &[&str]) -> Arc<PluginRegistry> {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("内置声明应可注册");
    let states = disabled
        .iter()
        .map(|id| (PluginId::new(*id).expect("valid builtin unit id"), false))
        .collect::<BTreeMap<_, _>>();
    Arc::new(
        builder
            .freeze_with_builtin_states(&states)
            .expect("启用视图应可冻结"),
    )
}

/// 启动真实 loopback server 并完成 RMCP 握手。
async fn start_client(
    services: McpServices,
) -> (
    RunningService<RoleClient, InitializeRequestParams>,
    McpServerHandle,
) {
    let server = McpServer::start(
        &Handle::current(),
        services,
        McpServerConfig::new("127.0.0.1:0".parse::<SocketAddr>().expect("valid loopback"))
            .expect("loopback is accepted"),
    )
    .await
    .expect("MCP server starts");
    let client = ClientInfo::default()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(server.endpoint()),
        ))
        .await
        .expect("MCP handshake succeeds");
    (client, server)
}

/// 关闭客户端并等待 server 回收，避免测试进程残留监听任务。
async fn stop(
    client: RunningService<RoleClient, InitializeRequestParams>,
    server: McpServerHandle,
) {
    let _ = client.cancel().await;
    let mut server = server;
    tokio::time::timeout(Duration::from_secs(5), server.shutdown())
        .await
        .expect("MCP shutdown stays bounded")
        .expect("MCP shutdown completes");
}

/// 以真实 RMCP client 调用帮助工具并取完整结构化 envelope。
async fn call_envelope(
    client: &RunningService<RoleClient, InitializeRequestParams>,
    name: &'static str,
    args: Value,
) -> Value {
    let arguments: Map<String, Value> =
        serde_json::from_value(args).expect("tool arguments are an object");
    client
        .peer()
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .expect("tool request reaches the server")
        .structured_content
        .expect("Kerminal returns structured content")
}

/// 以真实 RMCP client 调用帮助工具并取结构化 envelope 中的业务数据。
async fn call(
    client: &RunningService<RoleClient, InitializeRequestParams>,
    name: &'static str,
    args: Value,
) -> Value {
    let result = call_envelope(client, name, args).await;
    // 成功 envelope 统一包在 data 下；帮助断言只关心业务字段。
    result["data"].clone()
}

/// 返回当前 endpoint 真实暴露的工具名集合，供帮助表面做同一集合比较。
async fn tool_names(client: &RunningService<RoleClient, InitializeRequestParams>) -> Vec<String> {
    client
        .peer()
        .list_tools(None)
        .await
        .expect("tool discovery succeeds")
        .tools
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect()
}

/// 汇总所有区域的工具族 id，便于对裁剪结果做集合断言。
fn area_families(app: &Value) -> Vec<String> {
    app["areas"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|area| area["toolFamilies"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

/// 全启用时保持五区域结构、容器面板说明和既有推荐入口。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_enablement_keeps_guide_structure() {
    let services =
        McpServices::new(Arc::new(EmptySessionService)).with_plugins(registry_with_disabled(&[]));
    let (client, server) = start_client(services).await;

    let app = call(&client, "kerminal.help", json!({"topic":"app"})).await;
    assert_eq!(app["areas"].as_array().map(Vec::len), Some(5));
    assert!(app.get("containerPanel").is_some());
    let families = area_families(&app);
    for family in [
        "ssh",
        "credentials",
        "terminal",
        "agent-session",
        "sftp",
        "port-forward",
        "discovery",
        "history",
        "config",
    ] {
        assert!(families.iter().any(|entry| entry == family), "{family}");
    }
    assert!(
        app["mcpToolIds"]
            .as_array()
            .expect("tool ids")
            .iter()
            .any(|id| id == "ssh.command")
    );

    let operation = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"ssh-command"}),
    )
    .await;
    assert_eq!(operation["intent"], "ssh-command");
    assert!(
        operation["availableReferencedToolIds"]
            .as_array()
            .expect("available ids")
            .iter()
            .any(|id| id == "ssh.command")
    );

    let snapshot = call(&client, "kerminal.runtime_snapshot", json!({})).await;
    let snapshot_families = snapshot["toolFamilies"]
        .as_array()
        .expect("families")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    for family in ["ssh", "credentials", "sftp", "port-forward", "history"] {
        assert!(snapshot_families.contains(&family), "{family}");
    }

    let overview = call(&client, "kerminal.help", json!({"topic":"overview"})).await;
    assert!(
        overview["recommendedFirstCalls"]
            .as_array()
            .expect("recommended calls")
            .iter()
            .any(|id| id == "ssh.command")
    );
    stop(client, server).await;
}

/// 关闭 SSH、RDP 与容器族后，帮助与快照不再引用对应入口或工具。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_ssh_rdp_and_containers_trim_recommendations() {
    let services =
        McpServices::new(Arc::new(EmptySessionService)).with_plugins(registry_with_disabled(&[
            "kerminal.ssh",
            "kerminal.rdp",
            "kerminal.containers",
        ]));
    let (client, server) = start_client(services).await;

    let app = call(&client, "kerminal.help", json!({"topic":"app"})).await;
    let app_text = app.to_string();
    assert!(!app_text.contains("ssh.command"), "{app_text}");
    assert!(!app_text.contains("port-forward"), "{app_text}");
    assert!(!app_text.contains("\"credentials\""), "{app_text}");
    assert!(app.get("containerPanel").is_none());
    let families = area_families(&app);
    assert!(!families.iter().any(|family| family == "ssh"));
    assert!(!families.iter().any(|family| family == "port-forward"));
    assert!(!families.iter().any(|family| family == "credentials"));
    assert!(families.iter().any(|family| family == "terminal"));

    let ssh_intent = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"ssh-command"}),
    )
    .await;
    assert_eq!(ssh_intent["intent"], "overview");
    assert!(!ssh_intent.to_string().contains("ssh.command"));

    let port_intent = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"port-forward"}),
    )
    .await;
    assert_eq!(port_intent["intent"], "overview");
    assert!(!port_intent.to_string().contains("port_forward"));

    let credentials_intent = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"credentials"}),
    )
    .await;
    // 仅 SFTP 启用时带凭据主机保存仍可见，凭据意图保留但不得推荐不可见的 vault。
    assert_eq!(credentials_intent["intent"], "credentials");
    assert!(
        credentials_intent["availableReferencedToolIds"]
            .as_array()
            .expect("available ids")
            .iter()
            .any(|id| id == "kerminal.host.upsert_with_credential")
    );
    assert!(!credentials_intent.to_string().contains("kerminal.vault"));

    let snapshot = call(&client, "kerminal.runtime_snapshot", json!({})).await;
    let snapshot_families = snapshot["toolFamilies"]
        .as_array()
        .expect("families")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    for family in ["ssh", "credentials", "port-forward"] {
        assert!(!snapshot_families.contains(&family), "{family}");
    }
    assert!(snapshot_families.contains(&"sftp"));
    stop(client, server).await;
}

/// 关闭 SFTP 后不推荐传输工具，SSH 入口与终端核心能力不受影响。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_sftp_trims_transfer_recommendations() {
    let services = McpServices::new(Arc::new(EmptySessionService))
        .with_plugins(registry_with_disabled(&["kerminal.sftp"]));
    let (client, server) = start_client(services).await;

    let app = call(&client, "kerminal.help", json!({"topic":"app"})).await;
    let app_text = app.to_string();
    assert!(!app_text.contains("sftp.transfer"), "{app_text}");
    assert!(!area_families(&app).iter().any(|family| family == "sftp"));
    assert!(app_text.contains("ssh.command"));

    let sftp_intent = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"sftp"}),
    )
    .await;
    assert_eq!(sftp_intent["intent"], "overview");
    assert!(!sftp_intent.to_string().contains("sftp.transfer"));

    let snapshot = call(&client, "kerminal.runtime_snapshot", json!({})).await;
    let snapshot_families = snapshot["toolFamilies"]
        .as_array()
        .expect("families")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(!snapshot_families.contains(&"sftp"));
    assert!(snapshot_families.contains(&"ssh"));
    stop(client, server).await;
}

/// 会话 endpoint 在 Agent 单元关闭时不再推荐 target_context，会话意图回落到 overview。
/// 启用时保持原有 target_context 推荐，避免裁剪逻辑误伤正常会话。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_recommendations_follow_agents_unit() {
    let services = McpServices::new(Arc::new(EmptySessionService))
        .with_plugins(registry_with_disabled(&["kerminal.agents"]))
        .for_agent_session(AgentSessionId::new());
    let (client, server) = start_client(services).await;

    let app = call(&client, "kerminal.help", json!({"topic":"app"})).await;
    assert_eq!(
        app["sessionWorkspace"]["refreshTools"],
        json!(["terminal.list", "terminal.snapshot"])
    );
    assert!(!app.to_string().contains("target_context"));
    assert!(
        !area_families(&app)
            .iter()
            .any(|family| family == "agent-session")
    );

    let session_intent = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"session-terminal"}),
    )
    .await;
    assert_eq!(session_intent["intent"], "overview");
    assert!(!session_intent.to_string().contains("target_context"));
    stop(client, server).await;

    let services = McpServices::new(Arc::new(EmptySessionService))
        .with_plugins(registry_with_disabled(&[]))
        .for_agent_session(AgentSessionId::new());
    let (client, server) = start_client(services).await;
    let app = call(&client, "kerminal.help", json!({"topic":"app"})).await;
    assert_eq!(
        app["sessionWorkspace"]["refreshTools"],
        json!(["kerminal.agent.target_context"])
    );
    let session_intent = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"session-terminal"}),
    )
    .await;
    assert_eq!(session_intent["intent"], "session-terminal");
    assert!(
        session_intent["availableReferencedToolIds"]
            .as_array()
            .expect("available ids")
            .iter()
            .any(|id| id == "kerminal.agent.target_context")
    );
    stop(client, server).await;
}

/// 帮助的实体、writeToolIds 与 tool_help 必须与 tools/list 共用同一可见集合。
///
/// ssh 与 rdp 禁用但 sftp 启用时，带凭据主机保存仍可调用；凭据加密与
/// credentials 显示族同时隐藏，不能出现“目录已隐藏却仍在推荐”的漂移。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn help_surfaces_share_catalog_visibility_with_tools_list() {
    let services = McpServices::new(Arc::new(EmptySessionService))
        .with_plugins(registry_with_disabled(&["kerminal.ssh", "kerminal.rdp"]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client)
        .await
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert!(!names.contains("kerminal.vault.encrypt_secret"));
    assert!(names.contains("kerminal.host.upsert_with_credential"));

    let overview = call_envelope(&client, "kerminal.help", json!({"topic":"overview"})).await;
    let entity_ids = overview["entities"]
        .as_array()
        .expect("overview entities")
        .iter()
        .filter_map(|entity| entity["id"].as_str())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    assert_eq!(entity_ids, names, "overview entities match tools/list");
    let write_ids = overview["data"]["writeToolIds"]
        .as_array()
        .expect("write tool ids")
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    assert!(write_ids.contains("kerminal.host.upsert_with_credential"));
    assert!(!write_ids.contains("kerminal.vault.encrypt_secret"));
    let families = overview["data"]["runtimeToolFamilies"]
        .as_array()
        .expect("runtime tool families")
        .iter()
        .filter_map(|family| family["id"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(
        !families.contains("credentials"),
        "credentials family hides when SSH and RDP are both disabled"
    );
    assert!(families.contains("sftp"));

    let host_help = call(
        &client,
        "kerminal.help",
        json!({"topic":"tools","toolId":"kerminal.host.upsert_with_credential"}),
    )
    .await;
    assert_eq!(host_help["matchedToolCount"], 1, "{host_help}");
    let vault_help = call(
        &client,
        "kerminal.help",
        json!({"topic":"tools","toolId":"kerminal.vault.encrypt_secret"}),
    )
    .await;
    assert_eq!(vault_help["matchedToolCount"], 0, "{vault_help}");
    assert!(
        vault_help["missingToolIds"]
            .as_array()
            .expect("missing ids")
            .iter()
            .any(|id| id == "kerminal.vault.encrypt_secret")
    );
    stop(client, server).await;

    // 仅关闭 SSH、保留 RDP 时两个凭据工具都可发现，credentials 显示族也保留。
    let services = McpServices::new(Arc::new(EmptySessionService))
        .with_plugins(registry_with_disabled(&["kerminal.ssh"]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    for tool in [
        "kerminal.vault.encrypt_secret",
        "kerminal.host.upsert_with_credential",
    ] {
        assert!(names.iter().any(|name| name == tool), "{tool}: {names:?}");
    }
    let overview = call(&client, "kerminal.help", json!({"topic":"overview"})).await;
    assert!(
        overview["runtimeToolFamilies"]
            .as_array()
            .expect("runtime tool families")
            .iter()
            .any(|family| family["id"] == "credentials")
    );
    stop(client, server).await;
}

/// 全局 endpoint 上的 session-terminal 意图缺少主工具是范围限制，不是插件禁用。
///
/// 误报 capabilities disabled 会引导调用方去扩展页徒劳启用插件。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_session_intent_reports_endpoint_scope_not_disabled() {
    let services =
        McpServices::new(Arc::new(EmptySessionService)).with_plugins(registry_with_disabled(&[]));
    let (client, server) = start_client(services).await;
    let guide = call(
        &client,
        "kerminal.help",
        json!({"topic":"operation","intent":"session-terminal"}),
    )
    .await;
    assert_eq!(guide["intent"], "overview");
    let note = guide["note"].as_str().expect("fallback note");
    assert!(note.contains("global endpoint"), "{note}");
    assert!(!note.contains("capabilities are disabled"), "{note}");
    assert!(
        !guide["availableReferencedToolIds"]
            .as_array()
            .expect("available ids")
            .iter()
            .any(|id| id == "kerminal.agent.target_context")
    );
    stop(client, server).await;
}

/// SSH 禁用后帮助静态文案不再把 SSH 能力描述为工作区或容器面板能力。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_disabled_guide_stops_describing_ssh_capabilities() {
    let services = McpServices::new(Arc::new(EmptySessionService))
        .with_plugins(registry_with_disabled(&["kerminal.ssh"]));
    let (client, server) = start_client(services).await;
    let app = call(&client, "kerminal.help", json!({"topic":"app"})).await;
    assert!(
        app.get("containerPanel").is_none(),
        "container panel binds SSH hosts: {app}"
    );
    let terminal_purpose = app["areas"]
        .as_array()
        .expect("areas")
        .iter()
        .find(|area| area["id"] == "terminal-workspace")
        .and_then(|area| area["purpose"].as_str())
        .expect("terminal workspace purpose");
    assert!(!terminal_purpose.contains("SSH"), "{terminal_purpose}");
    assert!(!app.to_string().contains("ssh.command"), "{app}");
    stop(client, server).await;
}
