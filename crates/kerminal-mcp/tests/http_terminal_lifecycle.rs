//! 使用 RMCP HTTP client 验证后台终端工具的真实协议往返。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    agents::{AgentResumeCapability, AgentSessionView},
    preferences::{
        CONFIG_GUIDE, DefaultLocalShellProvider, ProfileDraft, ProfileRepository, ProfileRevision,
        ProfileService, VersionedProfile,
    },
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionChange, SessionFilter, SessionRuntimeMetadata,
        SessionService, SessionSubscription, ShellCommandOutcome, ShellCommandRequest,
        TerminalInputEvent, TerminalPalette, TerminalSearchRequest, TerminalSearchResult,
        TerminalSelection, UiCommandLineRequest,
    },
};
use kerminal_domain::{
    agents::{AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionScope},
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    preferences::{ConfigRevision, TerminalProfile},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use kerminal_mcp::{McpServer, McpServerConfig, McpServices};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, ClientInfo, InitializeRequestParams, PaginatedRequestParams, Tool,
    },
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    runtime::Handle,
    sync::{Mutex, watch},
    time::{Instant, timeout},
};

#[path = "support/agent_service.rs"]
mod agent_service;
#[path = "support/commands_service.rs"]
mod commands_service;

#[derive(Default)]
struct FakeSessionService {
    sessions: Mutex<HashMap<SessionId, FakeSession>>,
}

struct FakeSession {
    info: SessionInfo,
    output: String,
}

struct FakeProfileRepository;

struct FakeDefaultLocalShellProvider;

/// 构造只供真实 HTTP lifecycle test 使用的 profile resolver，避免读取用户配置或主机环境。
fn test_profile_service() -> ProfileService {
    ProfileService::new(
        Arc::new(FakeProfileRepository),
        Arc::new(FakeDefaultLocalShellProvider),
    )
}

#[async_trait]
impl ProfileRepository for FakeProfileRepository {
    /// 返回无显式默认 profile 的状态，以覆盖应用平台 shell fallback 的真实用例路径。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }

    /// 此 HTTP 测试只读取默认目标，不需要版本化编辑列表。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Err(AppError::unsupported("profile editor is outside this test"))
    }

    /// 此 HTTP 测试不按 profile ID 读取配置。
    async fn get_profile(&self, _id: &str) -> AppResult<Option<VersionedProfile>> {
        Err(AppError::unsupported("profile lookup is outside this test"))
    }

    /// 此 HTTP 测试不修改文件化配置。
    async fn save_profile(
        &self,
        _draft: ProfileDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported(
            "profile writes are outside this test",
        ))
    }

    /// 此 HTTP 测试不删除 profile。
    async fn delete_profile(
        &self,
        _id: &str,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        Err(AppError::unsupported(
            "profile deletion is outside this test",
        ))
    }

    /// 此 HTTP 测试不切换默认 profile。
    async fn set_default_profile(
        &self,
        _id: &str,
        _expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported(
            "profile default changes are outside this test",
        ))
    }
}

impl DefaultLocalShellProvider for FakeDefaultLocalShellProvider {
    /// 返回固定可断言的启动配置，证明 MCP 消费应用解析结果而非直接读取环境变量。
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        Ok(LocalSessionTarget {
            program: OsString::from("fake-cli"),
            args: vec![OsString::from("--profile-arg")],
            working_directory: Some("profile-default-cwd".into()),
            environment: BTreeMap::from([(
                OsString::from("PROFILE_TEST"),
                OsString::from("profile-value"),
            )]),
        })
    }
}

/// 验证桌面生产装配会一次列出所有缺失工具依赖，测试 server 仍可用窄 fake 验证单族协议。
#[test]
fn production_service_validation_lists_all_missing_capabilities() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()));
    let error = services
        .validate_complete()
        .expect_err("partial services must not be used by the desktop composition root");

    assert_eq!(
        error.missing_services(),
        &[
            "agents",
            "remote_connections",
            "host_management",
            "credentials",
            "transfers",
            "operations",
            "commands",
            "profiles",
            "configuration_validation",
            "task_executor",
        ]
    );
    assert!(error.to_string().contains("MCP 服务装配不完整"));
}

/// 以真实 listener 验证无 Origin 原生请求、精确同源请求及 DNS rebinding 拒绝边界。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_origin_policy_allows_native_clients_and_rejects_untrusted_headers() {
    let mut server = McpServer::start(
        &Handle::current(),
        McpServices::new(Arc::new(FakeSessionService::default()))
            .with_profiles(test_profile_service()),
        McpServerConfig::new("127.0.0.1:0".parse::<SocketAddr>().expect("valid loopback"))
            .expect("loopback is accepted"),
    )
    .await
    .expect("MCP server starts");
    let endpoint = server.endpoint().to_owned();
    let authority = endpoint
        .strip_prefix("http://")
        .and_then(|address| address.split_once('/').map(|(authority, _)| authority))
        .expect("server endpoint contains an HTTP authority");
    let allowed_origin = format!("http://{authority}");

    assert_eq!(
        post_initialize_status(&endpoint, authority, &[], initialize_body()).await,
        200,
        "native clients may omit Origin"
    );
    assert_eq!(
        post_initialize_status(
            &endpoint,
            authority,
            &[allowed_origin.as_str()],
            initialize_body(),
        )
        .await,
        200,
        "the exact endpoint origin is allowed"
    );
    for origins in [
        vec!["http://attacker.invalid"],
        vec!["null"],
        vec!["not-an-origin"],
        vec![allowed_origin.as_str(), allowed_origin.as_str()],
    ] {
        assert_eq!(
            post_initialize_status(&endpoint, authority, &origins, initialize_body()).await,
            403,
            "present foreign, null, malformed, or duplicate Origin values must be forbidden"
        );
    }
    assert_eq!(
        post_initialize_status(&endpoint, "attacker.invalid", &[], initialize_body(),).await,
        403,
        "a non-loopback Host must be forbidden"
    );

    tokio::time::timeout(Duration::from_secs(7), server.shutdown())
        .await
        .expect("MCP shutdown remains bounded after rejected requests")
        .expect("MCP shutdown completes");
}

/// 构造合法 MCP initialize 消息，隔离 HTTP 安全测试对 RMCP client helper 的依赖。
fn initialize_body() -> &'static str {
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"origin-test","version":"1.0.0"}}}"#
}

/// 用原生 TCP 发送一笔请求以控制 Host/Origin 的原始 HTTP 字段并断言真实响应状态。
async fn post_initialize_status(endpoint: &str, host: &str, origins: &[&str], body: &str) -> u16 {
    let (authority, path) = endpoint
        .strip_prefix("http://")
        .and_then(|value| value.split_once('/'))
        .expect("HTTP endpoint contains authority and path");
    let mut stream = TcpStream::connect(authority)
        .await
        .expect("connect to loopback MCP listener");
    let mut request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for origin in origins {
        request.push_str(&format!("Origin: {origin}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write raw MCP initialize request");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    timeout(Duration::from_secs(3), reader.read_line(&mut status_line))
        .await
        .expect("MCP listener responds before the test deadline")
        .expect("read HTTP response status");
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("HTTP response includes a numeric status")
}

/// 等到服务端开始读取请求体才返回连接；保留未完成 body 作为真实 graceful-shutdown 屏障，而非依赖调度时延。
///
/// @author kongweiguang
async fn open_incomplete_mcp_request(endpoint: &str) -> TcpStream {
    timeout(Duration::from_secs(3), async {
        let (authority, path) = endpoint
            .strip_prefix("http://")
            .and_then(|value| value.split_once('/'))
            .expect("HTTP endpoint contains authority and path");
        let mut stream = TcpStream::connect(authority)
            .await
            .expect("connect shutdown barrier to loopback MCP listener");
        let request = format!(
            "POST /{path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            initialize_body().len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write shutdown barrier request headers without the body");
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .await
            .expect("read shutdown barrier interim status");
        assert_eq!(status_line, "HTTP/1.1 100 Continue\r\n");
        let mut header_end = String::new();
        reader
            .read_line(&mut header_end)
            .await
            .expect("read shutdown barrier interim header boundary");
        assert_eq!(header_end, "\r\n");
        reader.into_inner()
    })
    .await
    .expect("MCP begins reading the controlled request body before the test deadline")
}

/// 固定核心目录经真实 HTTP 检查发现与拒绝边界，已移除工具不能靠猜名重新调用。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_core_catalog_rejects_every_removed_tool() {
    let expected_core = [
        "kerminal.help",
        "kerminal.runtime_snapshot",
        "kerminal.config.validate",
        "kerminal.host.upsert_with_credential",
        "kerminal.vault.encrypt_secret",
        "ssh.command",
        "terminal.create",
        "terminal.write",
        "terminal.snapshot",
        "terminal.resize",
        "terminal.list",
        "terminal.close",
        "sftp.transfer.enqueue",
        "sftp.transfer.list",
        "sftp.transfer.cancel",
        "sftp.transfer.retry",
        "port_forward.create",
        "port_forward.list",
        "port_forward.close",
        "history.search",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    let sessions = Arc::new(FakeSessionService::default());
    let mut server = McpServer::start(
        &Handle::current(),
        McpServices::new(sessions.clone()),
        McpServerConfig::new("127.0.0.1:0".parse().unwrap()).unwrap(),
    )
    .await
    .expect("server starts");
    let client = ClientInfo::default()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(server.endpoint()),
        ))
        .await
        .expect("handshake succeeds");
    let listed = client
        .peer()
        .list_tools(None)
        .await
        .expect("discovery succeeds");
    let names = listed
        .tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names, expected_core);
    assert_eq!(listed.tools.len(), 20);
    assert!(listed.next_cursor.is_none());
    assert_catalog_contract(&listed.tools);
    let help = call(&client, "kerminal.help", json!({})).await;
    assert_eq!(data(&help)["data"]["toolCounts"]["exposed"], 20);
    assert!(data(&help)["data"].get("optionalToolGroups").is_none());
    let config = call(&client, "kerminal.help", json!({"topic":"config"})).await;
    assert_eq!(data(&config)["data"]["markdown"], CONFIG_GUIDE);
    let ssh = call(&client, "kerminal.help", json!({"toolId":"ssh.command"})).await;
    let schema = &data(&ssh)["data"]["toolReference"][0]["inputSchema"];
    assert_eq!(schema["required"], json!(["command"]));
    assert!(schema["properties"].get("hostId").is_some());
    assert!(schema["properties"].get("groupName").is_some());
    assert_eq!(
        data(&ssh)["data"]["toolReference"][0]["exampleArguments"]["hostId"],
        "<saved-host-id>"
    );
    for arguments in [
        json!({"topic":"unknown"}),
        json!({"topic":"config","toolId":"ssh.command"}),
    ] {
        assert_eq!(
            call(&client, "kerminal.help", arguments).await.is_error,
            Some(true)
        );
    }
    let baseline: Value =
        serde_json::from_str(include_str!("fixtures/mcp_catalog_v0_3_45.json")).unwrap();
    let unavailable = baseline["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| !expected_core.contains(name))
        .collect::<Vec<_>>();
    assert_eq!(
        unavailable.len(),
        50,
        "49 deleted legacy IDs plus the scoped-only context tool"
    );
    for tool_id in unavailable {
        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new(tool_id.to_owned()))
            .await;
        assert!(
            result.is_err(),
            "{tool_id} must be rejected before dispatch"
        );
        let hidden = call(&client, "kerminal.help", json!({"toolId":tool_id})).await;
        assert_eq!(
            data(&hidden)["data"]["matchedToolCount"],
            0,
            "help must not advertise {tool_id}"
        );
    }
    assert!(sessions.sessions.lock().await.is_empty());
    client.cancel().await.expect("client closes");
    server
        .shutdown_until(Instant::now() + Duration::from_secs(5))
        .await
        .expect("server shuts down");
}

/// 通过最小 application fake 隔离 PTY；检验真实 HTTP 生命周期、超时保留 owner 与在线客户端关闭。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rmcp_http_runs_headless_terminal_lifecycle_and_shutdowns_with_client_open() {
    let sessions = Arc::new(FakeSessionService::default());
    let mut server = McpServer::start(
        &Handle::current(),
        McpServices::new(sessions.clone())
            .with_profiles(test_profile_service())
            .with_commands(commands_service::empty_commands_service(sessions.clone())),
        McpServerConfig::new("127.0.0.1:0".parse::<SocketAddr>().expect("valid loopback"))
            .expect("loopback is accepted"),
    )
    .await
    .expect("MCP server starts");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(server.endpoint()),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("MCP handshake succeeds");

    let mut tools = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            .peer()
            .list_tools(Some(
                PaginatedRequestParams::default().with_cursor(cursor.clone()),
            ))
            .await
            .expect("tool discovery succeeds");
        tools.extend(page.tools);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_catalog_contract(&tools);

    let capabilities = call(&client, "kerminal.help", json!({"topic":"overview"})).await;
    assert_eq!(data(&capabilities)["data"]["toolCounts"]["exposed"], 20);
    assert_eq!(
        data(&capabilities)["data"]["hostPolicy"]["approvalOwner"],
        "MCP host 决定确认、权限、hook 与审计策略。"
    );

    let config_guide = call(&client, "kerminal.help", json!({"topic":"config"})).await;
    assert_eq!(data(&config_guide)["data"]["markdown"], CONFIG_GUIDE);

    let app_guide = call(&client, "kerminal.help", json!({"topic":"app"})).await;
    assert_app_guide_layout_contract(&data(&app_guide)["data"]);

    let help = call(
        &client,
        "kerminal.help",
        json!({
            "toolId":"terminal.write", "includeSchemas":false
        }),
    )
    .await;
    assert_eq!(data(&help)["data"]["matchMode"], "toolId");
    assert_eq!(
        data(&help)["data"]["availableToolIds"],
        json!(["terminal.write"])
    );
    assert!(
        data(&help)["data"]["toolReference"][0]
            .get("inputSchema")
            .is_none()
    );

    let sftp_help = call(
        &client,
        "kerminal.help",
        json!({"family":"sftp","includeSchemas":false}),
    )
    .await;
    let sftp_help_data = data(&sftp_help);
    let sftp_ids = sftp_help_data["data"]["availableToolIds"]
        .as_array()
        .expect("SFTP family help returns its current tool IDs");
    for tool_id in [
        "sftp.transfer.enqueue",
        "sftp.transfer.list",
        "sftp.transfer.cancel",
        "sftp.transfer.retry",
    ] {
        assert!(
            sftp_ids.iter().any(|id| id == tool_id),
            "SFTP family help discovers {tool_id}"
        );
    }

    let guide = call(&client, "kerminal.help", json!({"intent":"terminal"})).await;
    assert_eq!(data(&guide)["data"]["intent"], "terminal");
    assert!(
        data(&guide)["data"]["workflow"]
            .as_array()
            .is_some_and(|steps| !steps.is_empty())
    );

    let container_guide = call(&client, "kerminal.help", json!({"intent":"container"})).await;
    let container_guide_result = data(&container_guide);
    let container_guide_data = &container_guide_result["data"];
    let available_tools = container_guide_data["availableReferencedToolIds"]
        .as_array()
        .expect("container guide lists available tool IDs");
    assert_eq!(available_tools, &vec![json!("ssh.command")]);
    assert!(container_guide_data["containerTextWriteWorkflow"].is_null());
    assert!(
        container_guide_data["workflow"]
            .as_array()
            .is_some_and(|steps| { steps.iter().all(|step| step["toolId"] == "ssh.command") })
    );

    let created = call(
        &client,
        "terminal.create",
        json!({ "target": "local", "cwd": "mcp-test-cwd", "cols": 100, "rows": 35 }),
    )
    .await;
    let created_envelope = data(&created);
    let created_data = &created_envelope["data"];
    assert_eq!(created_data["target"], "local");
    assert_eq!(created_data["headless"], true);
    assert_eq!(created_data["ui"]["tabCreated"], false);
    let session_id = created_data["sessionId"]
        .as_str()
        .expect("session id returned")
        .to_owned();
    assert_eq!(created_data["session"]["shell"], "fake-cli");
    assert_eq!(created_data["session"]["cwd"], "mcp-test-cwd");
    {
        let stored = sessions.sessions.lock().await;
        let session_id = SessionId::parse(&session_id).expect("session id parses");
        let local_target = stored
            .get(&session_id)
            .and_then(|session| match &session.info.target {
                SessionTarget::Local(target) => Some(target),
                _ => None,
            })
            .expect("the default profile creates a local target");
        assert_eq!(local_target.args, [OsString::from("--profile-arg")]);
        assert_eq!(
            local_target
                .environment
                .get(&OsString::from("PROFILE_TEST")),
            Some(&OsString::from("profile-value"))
        );
    }

    let runtime = call(&client, "kerminal.runtime_snapshot", json!({})).await;
    let runtime_data = data(&runtime);
    let listed = runtime_data["data"]["terminalSessions"]
        .as_array()
        .expect("terminal snapshot array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["sessionId"], session_id);
    assert_eq!(listed[0]["owner"], "mcp");
    assert_eq!(listed[0]["headless"], true);

    let missing_id = SessionId::new().to_string();
    let missing = call(
        &client,
        "terminal.write",
        json!({ "sessionId": missing_id, "data": "must-not-fallback" }),
    )
    .await;
    assert_eq!(data(&missing)["errorKind"], "not_found");

    let written = call(
        &client,
        "terminal.write",
        json!({ "sessionId": session_id, "data": "headless-marker" }),
    )
    .await;
    assert_eq!(data(&written)["data"]["bytesWritten"], 15);

    let secret_written = call(
        &client,
        "terminal.write",
        json!({ "sessionId": session_id, "data": " password=synthetic-secret" }),
    )
    .await;
    assert_eq!(data(&secret_written)["data"]["bytesWritten"], 26);

    let snapshot = call(
        &client,
        "terminal.snapshot",
        json!({ "sessionId": session_id, "maxBytes": 64 }),
    )
    .await;
    assert_eq!(
        data(&snapshot)["data"]["snapshot"]["data"],
        "headless-marker password=[已脱敏]",
    );
    assert_eq!(data(&snapshot)["data"]["snapshot"]["truncated"], false);
    assert_eq!(data(&snapshot)["data"]["snapshot"]["redacted"], true);
    let snapshot_session = &data(&snapshot)["data"]["session"];
    for field in [
        "id",
        "sessionId",
        "shell",
        "cwd",
        "pid",
        "targetRef",
        "targetToken",
        "shellIntegration",
        "target",
        "owner",
        "cols",
        "rows",
        "status",
        "lifecycle",
        "connectionState",
        "headless",
    ] {
        assert!(snapshot_session.get(field).is_some(), "missing {field}");
    }
    assert_eq!(snapshot_session["sessionId"], session_id);
    assert_eq!(snapshot_session["shell"], "fake-cli");
    assert_eq!(snapshot_session["cwd"], "mcp-test-cwd");
    assert_eq!(snapshot_session["owner"], "mcp");
    assert_eq!(snapshot_session["target"]["type"], "local");
    assert_eq!(snapshot_session["target"]["shell"], "fake-cli");
    assert_eq!(snapshot_session["target"]["cwd"], "mcp-test-cwd");
    assert_eq!(snapshot_session["cols"], 100);
    assert_eq!(snapshot_session["rows"], 35);
    assert_eq!(snapshot_session["status"], "running");
    assert_eq!(snapshot_session["lifecycle"], "running");
    assert_eq!(snapshot_session["connectionState"], "connected");
    assert_eq!(snapshot_session["headless"], true);
    assert_eq!(snapshot_session["shellIntegration"]["status"], "disabled");
    assert!(!data(&snapshot).to_string().contains("synthetic-secret"));

    let unicode_written = call(
        &client,
        "terminal.write",
        json!({ "sessionId": session_id, "data": "你好\n" }),
    )
    .await;
    let unicode_data = data(&unicode_written);
    assert_eq!(unicode_data["data"]["bytesWritten"], 7);
    assert_eq!(
        unicode_data["data"].as_object().map(serde_json::Map::len),
        Some(1),
        "terminal.write keeps its existing result shape"
    );
    {
        let sessions = sessions.sessions.lock().await;
        let session_id = SessionId::parse(&session_id).expect("session id parses");
        let raw_output = &sessions
            .get(&session_id)
            .expect("the acknowledged target session remains registered")
            .output;
        assert!(
            raw_output.ends_with("你好\n"),
            "the acknowledged UTF-8 bytes reach the same target session"
        );
    }
    let updated_snapshot = call(
        &client,
        "terminal.snapshot",
        json!({ "sessionId": session_id, "maxBytes": 128 }),
    )
    .await;
    assert_eq!(
        data(&updated_snapshot)["data"]["snapshot"]["data"],
        "headless-marker password=[已脱敏]\n",
        "snapshot redaction covers a Unicode suffix adjacent to a secret value"
    );
    assert!(
        !data(&updated_snapshot)
            .to_string()
            .contains("synthetic-secret")
    );

    let closed = call(
        &client,
        "terminal.close",
        json!({ "sessionId": session_id }),
    )
    .await;
    assert_eq!(data(&closed)["data"]["closed"], true);
    assert!(sessions.sessions.lock().await.is_empty());

    // 仅有在线 RMCP client 可能被同步清理完；100 Continue 屏障确保 listener 此时仍有真实未完成请求。
    let incomplete_request = open_incomplete_mcp_request(server.endpoint()).await;
    let expired_shutdown = server
        .shutdown_until(Instant::now() - Duration::from_millis(1))
        .await;
    // 断言前释放受控 body 并重试原 owner，失败用例也不会遗留测试连接或跳过正常资源回收。
    drop(incomplete_request);
    // RMCP client remains live here; retry must join its internal service guard before succeeding.
    tokio::time::timeout(
        Duration::from_secs(7),
        server.shutdown_until(Instant::now() + Duration::from_secs(6)),
    )
    .await
    .expect("MCP shutdown is bounded with a connected client")
    .expect("MCP shutdown completes");
    let address = server
        .endpoint()
        .trim_start_matches("http://")
        .split('/')
        .next()
        .expect("loopback address exists")
        .parse::<SocketAddr>()
        .expect("loopback address parses");
    assert!(TcpStream::connect(address).await.is_err());
    let _ = client.cancel().await;
    assert!(
        expired_shutdown.is_err(),
        "an admitted incomplete HTTP body must keep graceful shutdown pending past its deadline"
    );
}

/// 验证 Agent 范围工具仅使用 session 身份，内部恢复能力不扩展 MCP 接口。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_scoped_resize_and_close_require_matching_scope_id_without_target_acl() {
    let sessions = Arc::new(FakeSessionService::default());
    let agent_id = AgentSessionId::new();
    let agent_cli = sessions
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("fake-agent-cli"),
                args: Vec::new(),
                working_directory: Some("agent-workspace".into()),
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Agent {
                agent_session_id: agent_id.clone(),
            },
            size: TerminalSize::default(),
            initial_palette: None,
        })
        .await
        .expect("Agent CLI session is registered");
    let agent_record = AgentSession::new(
        agent_id.clone(),
        AgentProvider::Codex,
        None,
        "Scope test Agent".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "C:\\agent-workspace".to_owned(),
        "C:\\agent-workspace\\sessions\\scope-test".to_owned(),
        AgentSessionLaunch {
            command_label: "codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "C:\\agent-workspace\\sessions\\scope-test".to_owned(),
        },
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("fixture Agent record is valid");
    let agents = Arc::new(agent_service::FixedAgentService::new(AgentSessionView {
        session: agent_record,
        terminal: Some(agent_cli),
        resume_recovery: None,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
    }));
    let target_session = sessions
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("fake-ui-shell"),
                args: Vec::new(),
                working_directory: Some("workspace-cwd".into()),
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::default(),
            initial_palette: None,
        })
        .await
        .expect("explicit UI session is registered");
    let target_id = target_session.session_id.to_string();

    let mut server = McpServer::start(
        &Handle::current(),
        McpServices::new(sessions.clone()).with_agents(agents),
        McpServerConfig::new("127.0.0.1:0".parse::<SocketAddr>().expect("valid loopback"))
            .expect("loopback is accepted"),
    )
    .await
    .expect("MCP server starts");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(server.agent_endpoint(&agent_id)),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("Agent-scoped MCP client connects");
    let mismatched_agent_id = AgentSessionId::new();

    let listed = client
        .peer()
        .list_tools(None)
        .await
        .expect("scoped discovery succeeds");
    assert_eq!(listed.tools.len(), 21);
    assert!(
        listed
            .tools
            .iter()
            .any(|tool| tool.name == "kerminal.agent.target_context")
    );
    let overview = call(&client, "kerminal.help", json!({})).await;
    assert_eq!(data(&overview)["data"]["toolCounts"]["exposed"], 21);

    let rejected_resize = call(
        &client,
        "terminal.resize",
        json!({
            "sessionId": target_id,
            "agentSessionId": mismatched_agent_id,
            "cols": 132,
            "rows": 42
        }),
    )
    .await;
    assert_eq!(data(&rejected_resize)["errorKind"], "conflict");
    {
        let stored = sessions.sessions.lock().await;
        let target = stored
            .get(&target_session.session_id)
            .expect("a scope mismatch must leave the target session alive");
        assert_eq!(target.info.size.columns(), 80);
        assert_eq!(target.info.size.rows(), 24);
    }

    let accepted_resize = call(
        &client,
        "terminal.resize",
        json!({
            "sessionId": target_id,
            "agentSessionId": agent_id,
            "cols": 132,
            "rows": 42
        }),
    )
    .await;
    assert_eq!(data(&accepted_resize)["data"]["cols"], 132);
    assert_eq!(data(&accepted_resize)["data"]["rows"], 42);

    let rejected_close = call(
        &client,
        "terminal.close",
        json!({
            "sessionId": target_id,
            "agentSessionId": mismatched_agent_id
        }),
    )
    .await;
    assert_eq!(data(&rejected_close)["errorKind"], "conflict");
    assert!(
        sessions
            .sessions
            .lock()
            .await
            .contains_key(&target_session.session_id)
    );

    let accepted_close = call(&client, "terminal.close", json!({ "sessionId": target_id })).await;
    assert_eq!(data(&accepted_close)["data"]["closed"], true);
    assert!(
        !sessions
            .sessions
            .lock()
            .await
            .contains_key(&target_session.session_id)
    );

    let _ = client.cancel().await;
    server.shutdown().await.expect("MCP server shuts down");
}

/// 以结构化参数调用真实 RMCP client API，避免测试自行拼 JSON-RPC envelope。
async fn call(
    client: &RunningService<RoleClient, InitializeRequestParams>,
    name: &'static str,
    args: Value,
) -> rmcp::model::CallToolResult {
    let arguments: Map<String, Value> =
        serde_json::from_value(args).expect("tool arguments are an object");
    client
        .peer()
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .expect("tool request reaches the server")
}

/// 固定兼容区域与目标边界，同时确保默认工具布局不会被导航指南误报为不可调整。
///
/// @author kongweiguang
fn assert_app_guide_layout_contract(guide: &Value) {
    assert_eq!(guide["schemaVersion"], 1);
    let areas = guide["areas"]
        .as_array()
        .expect("app guide areas are an array");
    assert_eq!(areas.len(), 5);
    let actual_ids = areas
        .iter()
        .map(|area| area["id"].as_str().expect("area id is a string"))
        .collect::<Vec<_>>();
    assert_eq!(
        actual_ids,
        vec![
            "hosts",
            "terminal-workspace",
            "agent-and-tools",
            "bottom-tools",
            "configuration-workspace"
        ],
        "area identifiers and order remain stable"
    );

    for (id, expected_location) in [
        ("hosts", "left"),
        ("terminal-workspace", "center"),
        ("agent-and-tools", "right"),
        ("bottom-tools", "right"),
        ("configuration-workspace", "workspace"),
    ] {
        let area = areas
            .iter()
            .find(|area| area["id"] == id)
            .expect("expected workbench area is present");
        assert!(area.as_object().is_some());
        assert_eq!(area["location"], expected_location);
        assert!(
            area["purpose"]
                .as_str()
                .is_some_and(|purpose| !purpose.trim().is_empty()),
            "area purpose is present for {id}"
        );
        assert!(
            area["toolFamilies"].as_array().is_some(),
            "area toolFamilies is an array for {id}"
        );
    }

    let has_family = |area_id: &str, family: &str| {
        areas
            .iter()
            .find(|area| area["id"] == area_id)
            .and_then(|area| area["toolFamilies"].as_array())
            .is_some_and(|families| families.iter().any(|value| value == family))
    };
    for (area_id, family) in [
        ("hosts", "ssh"),
        ("terminal-workspace", "terminal"),
        ("agent-and-tools", "agent-session"),
        ("agent-and-tools", "sftp"),
        ("agent-and-tools", "ssh"),
        ("bottom-tools", "history"),
        ("configuration-workspace", "config"),
    ] {
        assert!(has_family(area_id, family), "{area_id} includes {family}");
    }

    let area_purpose = |area_id: &str| {
        areas
            .iter()
            .find(|area| area["id"] == area_id)
            .and_then(|area| area["purpose"].as_str())
            .expect("expected area purpose is present")
    };
    let tools_purpose = area_purpose("agent-and-tools");
    assert!(!tools_purpose.contains("Context"));
    assert!(!tools_purpose.contains("更多工作区"));
    let container_panel = &guide["containerPanel"];
    assert_eq!(container_panel["uiToolId"], "containers");
    assert_eq!(container_panel["location"], "right");
    assert_eq!(container_panel["fixedPosition"], false);
    for action in ["右键菜单", "顺序", "显示状态", "内容打开位置"] {
        assert!(
            container_panel["customization"]
                .as_str()
                .unwrap()
                .contains(action)
        );
    }
    assert_eq!(container_panel["defaultAfter"], "sftp");
    assert_eq!(
        container_panel["binding"],
        "当前活动 SSH pane 或文件 Tab 的已核实宿主"
    );
    assert!(
        container_panel["hostMode"]
            .as_str()
            .unwrap()
            .contains("SSH pane 管理对应宿主")
    );
    let container_mode = container_panel["containerMode"].as_str().unwrap();
    for host_binding in ["创建时的 SSH 宿主", "文件 Tab 保留真实宿主身份"] {
        assert!(container_mode.contains(host_binding));
    }
    let unavailable = container_panel["unavailable"].as_str().unwrap();
    for reason in ["无有效目标", "身份不一致", "本机", "Telnet", "串口"] {
        assert!(unavailable.contains(reason));
    }
    assert!(unavailable.contains("不回退到左栏选中的主机"));
    let history_purpose = area_purpose("bottom-tools");
    assert!(history_purpose.contains("右栏底部图标"));
    assert!(history_purpose.contains("兼容保留"));
    assert!(history_purpose.contains("不表示终端底部工具区"));
    assert_eq!(
        guide["sessionWorkspace"]["refreshTools"],
        json!(["terminal.list", "terminal.snapshot"])
    );
    let context_files = guide["sessionWorkspace"]["readFirst"]
        .as_array()
        .expect("Agent context files remain discoverable");
    for file in [
        "context/mcp-endpoint.json",
        "context/target-binding.json",
        "context/terminal-snapshot.json",
    ] {
        assert!(context_files.iter().any(|value| value == file));
    }
}

/// 固定核心目录仍核对独立旧版 schema；统一帮助和 SSH 定位入口按新契约验证。
fn assert_catalog_contract(actual: &[Tool]) {
    let current: Value = serde_json::from_str(include_str!("../resources/tool_catalog.json"))
        .expect("current MCP catalog is valid JSON");
    let baseline: Value = serde_json::from_str(include_str!("fixtures/mcp_catalog_v0_3_45.json"))
        .expect("legacy fixture remains independent");
    let expected = current["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| tool["agentScoped"] != true)
        .collect::<Vec<_>>();
    let names = actual
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(actual.len(), 20);
    assert_eq!(
        names,
        expected
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>()
    );
    for (tool, expected) in actual.iter().zip(expected) {
        assert_eq!(
            serde_json::to_value(&tool.input_schema).unwrap(),
            expected["inputSchema"]
        );
        assert_eq!(
            serde_json::to_value(&tool.annotations).unwrap(),
            expected["annotations"]
        );
        if tool.name != "kerminal.help" && tool.name != "ssh.command" {
            let legacy = baseline["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|old| old["name"] == tool.name.as_ref())
                .expect("retained tool has a legacy contract");
            assert_eq!(
                expected["inputSchema"], legacy["inputSchema"],
                "{} keeps its schema",
                tool.name
            );
            assert_eq!(
                expected["annotations"], legacy["annotations"],
                "{} keeps its annotations",
                tool.name
            );
        }
    }
}

/// 从 RMCP 结构化响应读取旧版兼容 envelope，明确区分执行错误与 HTTP 错误。
fn data(result: &rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("Kerminal returns structured content")
}

#[async_trait]
impl SessionService for FakeSessionService {
    /// 在测试注册表中创建 MCP-owned session；不会构造 UI owner 或真实子进程。
    async fn create(&self, request: CreateSessionRequest) -> AppResult<SessionInfo> {
        let info = SessionInfo {
            session_id: SessionId::new(),
            target: request.target,
            owner: request.owner,
            size: request.size,
            lifecycle: SessionLifecycle::Running,
            exit_status: None,
        };
        self.sessions.lock().await.insert(
            info.session_id.clone(),
            FakeSession {
                info: info.clone(),
                output: String::new(),
            },
        );
        Ok(info)
    }

    /// 先拒绝已取消请求，之后复用同一 fake 创建路径验证 supervisor 接入。
    async fn create_cancellable(
        &self,
        request: CreateSessionRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test session create cancelled"));
        }
        self.create(request).await
    }

    /// 只列出 fake registry 内容，用于确认 close 后资源已释放。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        Ok(self
            .sessions
            .lock()
            .await
            .values()
            .map(|session| session.info.clone())
            .collect())
    }

    /// Fake transport 不伪造本地子进程 PID 或 OSC 133 状态，保持元数据契约与测试事实一致。
    async fn runtime_metadata(&self, session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        use kerminal_domain::connections::RemoteShellDialect;
        self.sessions
            .lock()
            .await
            .contains_key(&session_id)
            .then_some(SessionRuntimeMetadata {
                process_id: None,
                shell_dialect: RemoteShellDialect::Unknown,
                shell_integration_enabled: false,
                shell_integration_observed: false,
                prompt_ready: false,
            })
            .ok_or_else(|| AppError::not_found("session not found"))
    }

    /// 将输入写入指定 session 的 fake 输出缓存；失效 ID 不会回退到其它会话。
    async fn write(&self, session_id: SessionId, bytes: Vec<u8>) -> AppResult<()> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .ok_or_else(|| AppError::not_found("session not found"))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| AppError::invalid("terminal input is not UTF-8"))?;
        session.output.push_str(&text);
        Ok(())
    }

    /// 本测试不模拟 GPUI 键盘编码路径。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test terminal input event"))
    }

    /// MCP 生命周期 fake 不实现 UI owner 准入，必须拒绝而不能降级成无绑定写入。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<kerminal_application::sessions::TerminalCommandContext> {
        Err(AppError::unsupported(
            "test fake does not implement visible UI command-line submission",
        ))
    }

    /// 仅更新指定 fake session 的网格尺寸。
    async fn resize(&self, session_id: SessionId, size: TerminalSize) -> AppResult<()> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .ok_or_else(|| AppError::not_found("session not found"))?;
        session.info.size = size;
        Ok(())
    }

    /// Fake 不渲染颜色，但实现 required palette port 以保持对真实 SessionService 的契约覆盖。
    async fn set_palette(&self, session_id: SessionId, _palette: TerminalPalette) -> AppResult<()> {
        self.sessions
            .lock()
            .await
            .contains_key(&session_id)
            .then_some(())
            .ok_or_else(|| AppError::not_found("session not found"))
    }

    /// 测试 fake 不裁剪终端缓冲区，只校验显式会话仍存在并回传请求值。
    async fn set_scrollback_limit(&self, session_id: SessionId, lines: usize) -> AppResult<usize> {
        self.sessions
            .lock()
            .await
            .contains_key(&session_id)
            .then_some(lines)
            .ok_or_else(|| AppError::not_found("session not found"))
    }

    /// 本测试关注 MCP 纯文本路径，不生成 GPUI 渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        Err(AppError::unsupported("test render snapshot"))
    }

    /// 返回受限窗口内的纯文本结果，以验证 MCP 使用显式 byte budget。
    async fn plaintext_snapshot(
        &self,
        session_id: SessionId,
        request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| AppError::not_found("session not found"))?;
        let mut text = session.output.clone();
        let budget = request.max_bytes.unwrap_or(usize::MAX);
        if text.len() > budget {
            let mut start = text.len() - budget;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            text = text[start..].to_owned();
        }
        let first_line = session.output.len().saturating_sub(text.len()) as i64;
        Ok(PlainTextSnapshot {
            session_id,
            line_count: text.lines().count(),
            first_line,
            total_scrollback_lines: session.output.lines().count(),
            truncated: text.len() < session.output.len(),
            text,
        })
    }

    /// 测试 fake 不维护滚动视口状态。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        Err(AppError::unsupported("test scroll"))
    }

    /// MCP fake 保留后台滚动契约，不冒充可见 UI/Agent owner 绑定。
    ///
    /// @author kongweiguang
    async fn scroll_ui(
        &self,
        _binding: kerminal_application::sessions::UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test UI-bound scroll"))
    }

    /// 测试 fake 不维护网格选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test selection"))
    }

    /// 测试 fake 没有终端 selection 内容。
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

    /// 测试 fake 没有远程可重连目标，并且先尊重 supervisor 的取消信号。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test reconnect cancelled"));
        }
        Err(AppError::unsupported("test reconnect"))
    }

    /// 关闭只会移除显式 ID，避免遗留测试资源。
    async fn close(&self, session_id: SessionId) -> AppResult<()> {
        self.sessions
            .lock()
            .await
            .remove(&session_id)
            .map(|_| ())
            .ok_or_else(|| AppError::not_found("session not found"))
    }

    /// Fake 关闭立即释放指定 ID，匹配 production 的绝对 deadline 端口且不丢弃收尾 future。
    async fn close_until(
        &self,
        session_id: SessionId,
        _deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        self.close(session_id).await
    }

    /// 本测试不共享其它进程创建的 runtime session。
    async fn close_all(&self) -> AppResult<()> {
        self.sessions.lock().await.clear();
        Ok(())
    }

    /// 测试 fake 不广播 UI 更新。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        let (_sender, receiver) = watch::channel(SessionChange {
            revision: 0,
            kind: kerminal_application::sessions::SessionChangeKind::Output,
        });
        Ok(SessionSubscription::from_receiver(receiver))
    }

    /// 该 MCP HTTP fake 不实现交互命令，只显式报告能力边界以满足完整 application port。
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test shell command cancelled"));
        }
        Err(AppError::unsupported("test shell command"))
    }
}
