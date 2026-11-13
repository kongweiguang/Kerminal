//! 验证插件启用视图同时约束 MCP 目录发现与请求级门禁。
//!
//! 测试使用真实 RMCP HTTP 往返，确保“隐藏”和“结构化拒绝”都经过与生产相同的目录、
//! 分派和 application fake 边界，而不是只验证内部映射函数。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, RemoteConnectionLease, RemoteConnectionService,
        credentials::{CredentialService, CredentialVaultRepository, CredentialVaultSaveMetadata},
        management::{
            HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroupDeletionReceipt,
            HostGroupDraft, HostGroupSnapshot, HostManagementRepository, HostManagementService,
            HostRouteSettings, ManagedHost,
        },
    },
    plugins::{PluginId, PluginRegistry, PluginRegistryBuilder},
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionFilter, SessionRuntimeMetadata,
        SessionService, SessionSubscription, ShellCommandOutcome, ShellCommandRequest,
        TerminalCommandContext, TerminalInputEvent, TerminalPalette, TerminalSearchRequest,
        TerminalSearchResult, TerminalSelection, UiCommandLineRequest, UiSessionBinding,
    },
    transfers::{
        TransferBackend, TransferCleanupState, TransferExecutionContext, TransferExecutionError,
        TransferExecutionFailure, TransferExecutionResult, TransferQueueOptions, TransferService,
    },
};
use kerminal_domain::{
    connections::{CredentialRef, HostEndpoint, RemoteProtocol, credentials::CredentialReference},
    files::{ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind, TransferRequest},
    identity::{AgentSessionId, HostId, SessionId},
    preferences::ConfigRevision,
    sessions::{
        SerialSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        SshSessionTarget, TelnetSessionTarget, TerminalSize,
    },
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
use tokio::{runtime::Handle, sync::Mutex};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[path = "support/commands_service.rs"]
mod commands_service;

/// 只记录 create/list 的内存会话服务；其余端口显式拒绝，避免测试伪装出未验证能力。
#[derive(Default)]
struct FakeSessionService {
    sessions: Mutex<Vec<SessionInfo>>,
}

impl FakeSessionService {
    /// 直接预置目标会话，用于验证按既有 target 判定的请求级门禁。
    async fn seed(&self, target: SessionTarget) -> SessionId {
        let size = TerminalSize::new(80, 24, 0, 0).expect("valid terminal size");
        let info = SessionInfo {
            session_id: SessionId::new(),
            target,
            owner: SessionOwner::Mcp,
            size,
            lifecycle: SessionLifecycle::Running,
            exit_status: None,
        };
        let session_id = info.session_id.clone();
        self.sessions.lock().await.push(info);
        session_id
    }

    /// 返回当前会话数量，供“拒绝后不得创建会话”的断言使用。
    async fn count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

#[async_trait]
impl SessionService for FakeSessionService {
    /// 仅登记输入目标，不启动真实 PTY；本地与 SSH 的准入已由 MCP 门禁先行判定。
    async fn create(&self, request: CreateSessionRequest) -> AppResult<SessionInfo> {
        let info = SessionInfo {
            session_id: SessionId::new(),
            target: request.target,
            owner: request.owner,
            size: request.size,
            lifecycle: SessionLifecycle::Running,
            exit_status: None,
        };
        self.sessions.lock().await.push(info.clone());
        Ok(info)
    }

    /// 先尊重取消，再复用同一登记路径，保持 create_cancellable 的 supervisor 语义。
    async fn create_cancellable(
        &self,
        request: CreateSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test session create cancelled"));
        }
        self.create(request).await
    }

    /// 只返回测试登记的会话快照。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        Ok(self.sessions.lock().await.clone())
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
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
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

    /// 测试 fake 关闭只移除登记项，供资源回收断言使用。
    async fn close(&self, session_id: SessionId) -> AppResult<()> {
        let mut sessions = self.sessions.lock().await;
        let before = sessions.len();
        sessions.retain(|session| session.session_id != session_id);
        if sessions.len() == before {
            return Err(AppError::not_found("session not found"));
        }
        Ok(())
    }

    /// 测试 fake 立即完成关闭，不维护独立 deadline。
    async fn close_until(
        &self,
        session_id: SessionId,
        _deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        self.close(session_id).await
    }

    /// 测试 fake 清空全部登记项。
    async fn close_all(&self) -> AppResult<()> {
        self.sessions.lock().await.clear();
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

/// 只提供只读目录的主机仓储；写入路径显式拒绝，确保门禁测试不触碰真实配置。
struct FakeHostRepository {
    hosts: Vec<ManagedHost>,
}

#[async_trait]
impl HostManagementRepository for FakeHostRepository {
    /// 门禁测试不写入主机文件。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Ok(())
    }

    /// 返回固定主机集合，供传输/凭据门禁解析目标协议。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(self.hosts.clone())
    }

    /// 按稳定身份查找主机，未知身份保持 None。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self.hosts.iter().find(|host| &host.id == host_id).cloned())
    }

    /// 门禁测试不保存主机。
    async fn save_host(
        &self,
        _host_id: HostId,
        _draft: HostDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("test host save"))
    }

    /// 门禁测试不删除主机。
    async fn delete_host(
        &self,
        _host_id: &HostId,
        _expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("test host delete"))
    }

    /// 返回空分组目录，避免文件访问。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot {
            groups: Vec::new(),
            revision: None,
        })
    }

    /// 门禁测试不写分组。
    async fn save_group(
        &self,
        _draft: HostGroupDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("test group save"))
    }

    /// 门禁测试不删除分组。
    async fn delete_group_and_reassign(
        &self,
        _group_id: String,
        _destination_group_id: Option<String>,
        _expected_group_revision: ConfigRevision,
        _expected_hosts: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported("test group delete"))
    }
}

/// 门禁测试不建立真实连接。
struct UnusedRemoteConnections;

#[async_trait]
impl RemoteConnectionService for UnusedRemoteConnections {
    /// 连接能力被门禁拒绝后不应到达这里；到达则显式暴露测试装配缺口。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("test remote connections"))
    }
}

/// 门禁测试不写入 Vault。
struct UnusedVault;

#[async_trait]
impl CredentialVaultRepository for UnusedVault {
    /// 凭据写入被门禁拒绝后不应到达这里。
    async fn save(
        &self,
        _reference: CredentialReference,
        _secret: Zeroizing<String>,
    ) -> AppResult<CredentialVaultSaveMetadata> {
        Err(AppError::unsupported("test vault save"))
    }

    /// 门禁测试不读取凭据存在性。
    async fn contains(&self, _reference: &CredentialRef) -> AppResult<bool> {
        Ok(false)
    }

    /// 门禁测试不删除凭据。
    async fn remove(&self, _reference: &CredentialRef) -> AppResult<bool> {
        Ok(false)
    }
}

/// 构造带指定禁用单元的真实冻结视图；其余内置单元保持启用。
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

/// 传输重试门禁测试的后端：立即以可重试连接失败结束，不执行任何真实 IO。
struct FailingTransferBackend;

#[async_trait]
impl TransferBackend for FailingTransferBackend {
    /// 接受任意端点组合，让测试聚焦重试前的协议门禁而不是后端能力协商。
    fn supports(&self, _request: &TransferRequest) -> bool {
        true
    }

    /// 立即返回可重试失败，任务记录保留原请求供 retry 门禁解析。
    async fn execute(
        &self,
        _context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        Err(TransferExecutionError::Failed(
            TransferExecutionFailure::new(
                TransferFailureKind::Connection,
                true,
                false,
                None,
                None,
                TransferCleanupState::Released,
            ),
        ))
    }
}

/// 构造只含可控失败后端的内存队列；测试结束由调用方显式关闭。
fn failing_transfer_service() -> TransferService {
    TransferService::new(
        vec![Arc::new(FailingTransferBackend) as Arc<dyn TransferBackend>],
        Handle::current(),
        TransferQueueOptions::default(),
    )
    .expect("default transfer queue options are valid")
}

/// 构造可被传输/凭据门禁解析的固定协议主机。
fn host_with_protocol(id: &str, protocol: RemoteProtocol) -> ManagedHost {
    ManagedHost {
        id: HostId::parse(id).expect("valid host id"),
        group_id: None,
        name: "gated-host".to_owned(),
        note: None,
        protocol,
        endpoint: HostEndpoint {
            host: "example.test".to_owned(),
            port: 22,
            username: "demo".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Password,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: Some(60_000),
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
        revision: ConfigRevision::from_sha256("test-revision".to_owned()),
    }
}

/// 构造可被传输/凭据门禁解析的 SSH 主机。
fn ssh_host(id: &str) -> ManagedHost {
    host_with_protocol(id, RemoteProtocol::Ssh)
}

/// 构造 SFTP 协议主机，用于验证只启用 sftp 时带凭据保存可进入请求级门禁。
fn sftp_host(id: &str) -> ManagedHost {
    host_with_protocol(id, RemoteProtocol::Sftp)
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
    mut server: McpServerHandle,
) {
    let _ = client.cancel().await;
    tokio::time::timeout(Duration::from_secs(5), server.shutdown())
        .await
        .expect("MCP shutdown stays bounded")
        .expect("MCP shutdown completes");
}

/// 返回当前 endpoint 真实暴露的工具名集合。
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

/// 以真实 RMCP client 调用工具并取结构化 envelope。
async fn call(
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

/// 断言结构化结果为 plugin_disabled，并点名至少一个应当启用的单元。
fn assert_plugin_disabled(value: &Value, unit: &str) {
    assert_eq!(value["errorKind"], "plugin_disabled", "{value}");
    assert_eq!(value["recoverable"], false, "{value}");
    let summary = value["summary"]
        .as_str()
        .expect("plugin_disabled has summary");
    assert!(summary.contains(unit), "summary names {unit}: {summary}");
}

/// 断言 plugin_disabled 携带完整且稳定的单元视图，避免凭据工具返回空 requiredUnits。
fn assert_disabled_units(value: &Value, required: &[&str], disabled: &[&str]) {
    assert_eq!(value["errorKind"], "plugin_disabled", "{value}");
    let required_units = value["data"]["requiredUnits"]
        .as_array()
        .expect("plugin_disabled has requiredUnits");
    let disabled_units = value["data"]["disabledUnits"]
        .as_array()
        .expect("plugin_disabled has disabledUnits");
    assert_eq!(
        required_units
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>(),
        required.to_vec(),
        "{value}"
    );
    assert_eq!(
        disabled_units
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>(),
        disabled.to_vec(),
        "{value}"
    );
}

/// 全启用时全局 endpoint 保持 20 个核心工具，会话 endpoint 增加 Agent 上下文工具。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_enablement_keeps_catalog_baseline_and_agent_scope() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_plugins(registry_with_disabled(&[]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    let expected = [
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
    .map(str::to_owned)
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    assert_eq!(names.len(), 20);
    assert!(
        !names
            .iter()
            .any(|name| name == "kerminal.agent.target_context")
    );

    let unknown = client
        .peer()
        .call_tool(CallToolRequestParams::new("kerminal.removed".to_owned()))
        .await;
    assert!(unknown.is_err(), "unknown tools stay METHOD_NOT_FOUND");
    stop(client, server).await;

    let scoped = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_plugins(registry_with_disabled(&[]))
        .for_agent_session(AgentSessionId::new());
    let (client, server) = start_client(scoped).await;
    let scoped_names = tool_names(&client).await;
    assert_eq!(scoped_names.len(), 21);
    assert!(
        scoped_names
            .iter()
            .any(|name| name == "kerminal.agent.target_context")
    );
    stop(client, server).await;
}

/// 禁用 SSH 时隐藏 SSH/端口转发工具，本地终端保持可用，SSH 目标与既有 SSH 会话写入被拒绝。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_ssh_hides_family_but_keeps_local_terminal() {
    let sessions = Arc::new(FakeSessionService::default());
    let services =
        McpServices::new(sessions.clone()).with_plugins(registry_with_disabled(&["kerminal.ssh"]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    assert!(!names.iter().any(|name| name == "ssh.command"));
    assert!(!names.iter().any(|name| name.starts_with("port_forward.")));
    assert!(names.iter().any(|name| name == "sftp.transfer.enqueue"));

    assert_plugin_disabled(
        &call(&client, "ssh.command", json!({ "command": "uptime" })).await,
        "kerminal.ssh",
    );

    let local = call(
        &client,
        "terminal.create",
        json!({ "target": "local", "shell": "fake-shell" }),
    )
    .await;
    assert_eq!(
        local.get("errorKind"),
        None,
        "local terminal stays usable: {local}"
    );
    assert_eq!(sessions.count().await, 1);

    let ssh_create = call(
        &client,
        "terminal.create",
        json!({ "target": "ssh", "hostId": "gated-ssh-host" }),
    )
    .await;
    assert_plugin_disabled(&ssh_create, "kerminal.ssh");
    assert_eq!(
        sessions.count().await,
        1,
        "SSH target must not create a session"
    );

    let ssh_session = sessions
        .seed(SessionTarget::Ssh(SshSessionTarget {
            host_id: HostId::parse("gated-ssh-host").expect("valid host id"),
            working_directory: None,
            command: None,
        }))
        .await;
    assert_plugin_disabled(
        &call(
            &client,
            "terminal.write",
            json!({ "sessionId": ssh_session.as_str(), "data": "ls\n" }),
        )
        .await,
        "kerminal.ssh",
    );
    stop(client, server).await;
}

/// 禁用 SFTP 时隐藏并拒绝传输族；SSH 主机端点额外要求 ssh，本地端点不受影响。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_sftp_hides_transfers_and_ssh_host_adds_ssh_requirement() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_plugins(registry_with_disabled(&["kerminal.sftp"]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    assert!(!names.iter().any(|name| name.starts_with("sftp.transfer.")));
    assert_plugin_disabled(
        &call(
            &client,
            "sftp.transfer.enqueue",
            json!({
                "source": { "type": "local", "path": "C:/tmp/source.bin" },
                "destination": { "type": "local", "path": "C:/tmp/destination.bin" },
                "kind": "file",
                "conflictPolicy": "overwrite"
            }),
        )
        .await,
        "kerminal.sftp",
    );
    stop(client, server).await;

    let sessions = Arc::new(FakeSessionService::default());
    let host_repository = Arc::new(FakeHostRepository {
        hosts: vec![ssh_host("gated-ssh-host")],
    });
    let services = McpServices::new(sessions)
        .with_connections(
            Arc::new(UnusedRemoteConnections),
            HostManagementService::new(host_repository),
            CredentialService::new(Arc::new(UnusedVault)),
        )
        .with_plugins(registry_with_disabled(&["kerminal.ssh"]));
    let (client, server) = start_client(services).await;

    assert_plugin_disabled(
        &call(
            &client,
            "sftp.transfer.enqueue",
            json!({
                "source": { "type": "remote", "hostId": "gated-ssh-host", "path": "/tmp/source.bin" },
                "destination": { "type": "local", "path": "C:/tmp/destination.bin" },
                "kind": "file",
                "conflictPolicy": "overwrite"
            }),
        )
        .await,
        "kerminal.ssh",
    );
    let local_only = call(
        &client,
        "sftp.transfer.enqueue",
        json!({
            "source": { "type": "local", "path": "C:/tmp/source.bin" },
            "destination": { "type": "local", "path": "C:/tmp/destination.bin" },
            "kind": "file",
            "conflictPolicy": "overwrite"
        }),
    )
    .await;
    assert_ne!(
        local_only["errorKind"], "plugin_disabled",
        "local transfer must not depend on SSH: {local_only}"
    );
    stop(client, server).await;
}

/// 禁用 ports 时隐藏端口转发族，即使 ssh 仍启用。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_ports_hides_port_forward_family() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_plugins(registry_with_disabled(&["kerminal.ports"]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    assert!(!names.iter().any(|name| name.starts_with("port_forward.")));
    assert!(names.iter().any(|name| name == "ssh.command"));
    assert_plugin_disabled(
        &call(&client, "port_forward.list", json!({})).await,
        "kerminal.ports",
    );
    stop(client, server).await;
}

/// 禁用 logs 时隐藏历史查询；启用 logs 但禁用 commands 时片段来源额外被拒绝，本地默认来源放行。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_gate_follows_logs_commands_and_remote_targets() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_plugins(registry_with_disabled(&["kerminal.logs"]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    assert!(!names.iter().any(|name| name == "history.search"));
    assert_plugin_disabled(
        &call(&client, "history.search", json!({})).await,
        "kerminal.logs",
    );
    stop(client, server).await;

    let sessions = Arc::new(FakeSessionService::default());
    let services = McpServices::new(sessions.clone())
        .with_commands(commands_service::empty_commands_service(sessions))
        .with_plugins(registry_with_disabled(&["kerminal.commands"]));
    let (client, server) = start_client(services).await;

    assert_plugin_disabled(
        &call(&client, "history.search", json!({ "source": "snippet" })).await,
        "kerminal.commands",
    );
    let local_user = call(&client, "history.search", json!({ "source": "user" })).await;
    assert_ne!(
        local_user["errorKind"], "plugin_disabled",
        "local user history does not depend on commands: {local_user}"
    );
    stop(client, server).await;
}

/// 禁用 agents 时即使处于会话 endpoint，Agent 上下文工具也不可见且调用被拒绝。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_agents_hides_agent_context() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_plugins(registry_with_disabled(&["kerminal.agents"]))
        .for_agent_session(AgentSessionId::new());
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    assert!(
        !names
            .iter()
            .any(|name| name == "kerminal.agent.target_context")
    );
    assert_plugin_disabled(
        &call(
            &client,
            "kerminal.agent.target_context",
            json!({
                "agentSessionId": "gated-agent"
            }),
        )
        .await,
        "kerminal.agents",
    );
    stop(client, server).await;
}

/// 凭据工具按最终主机协议与凭据种类判定；禁用 ssh 默认阻止新建主机与 SSH 凭据。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_tools_gate_by_protocol_and_kind() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_connections(
            Arc::new(UnusedRemoteConnections),
            HostManagementService::new(Arc::new(FakeHostRepository { hosts: Vec::new() })),
            CredentialService::new(Arc::new(UnusedVault)),
        )
        .with_plugins(registry_with_disabled(&["kerminal.ssh"]));
    let (client, server) = start_client(services).await;

    assert_plugin_disabled(
        &call(
            &client,
            "kerminal.vault.encrypt_secret",
            json!({
                "kind": "ssh-host",
                "hostId": "gated-host",
                "scope": "target",
                "material": "password",
                "plaintext": "test-secret"
            }),
        )
        .await,
        "kerminal.ssh",
    );
    let rdp_secret = call(
        &client,
        "kerminal.vault.encrypt_secret",
        json!({
            "kind": "rdp-host",
            "hostId": "gated-host",
            "scope": "target",
            "material": "password",
            "plaintext": "test-secret"
        }),
    )
    .await;
    assert_ne!(
        rdp_secret["errorKind"], "plugin_disabled",
        "RDP credential does not depend on SSH: {rdp_secret}"
    );
    assert_plugin_disabled(
        &call(
            &client,
            "kerminal.host.upsert_with_credential",
            json!({
                "name": "gated",
                "host": "example.test",
                "username": "demo",
                "password": "test-secret"
            }),
        )
        .await,
        "kerminal.ssh",
    );
    stop(client, server).await;

    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_plugins(registry_with_disabled(&["kerminal.rdp"]));
    let (client, server) = start_client(services).await;
    assert_plugin_disabled(
        &call(
            &client,
            "kerminal.vault.encrypt_secret",
            json!({
                "kind": "rdp-host",
                "hostId": "gated-host",
                "scope": "target",
                "material": "password",
                "plaintext": "test-secret"
            }),
        )
        .await,
        "kerminal.rdp",
    );
    stop(client, server).await;
}

/// 凭据工具的发现可见性按任一候选协议族判定，请求级仍按真实目标协议拒绝。
///
/// ssh 与 rdp 同时禁用但 sftp 启用时，带凭据主机保存仍可见；凭据加密不可见。
/// 三族全禁用时两者都从目录消失，plugin_disabled 必须给出完整候选与禁用单元。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_visibility_uses_any_of_protocol_families() {
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_connections(
            Arc::new(UnusedRemoteConnections),
            HostManagementService::new(Arc::new(FakeHostRepository { hosts: Vec::new() })),
            CredentialService::new(Arc::new(UnusedVault)),
        )
        .with_plugins(registry_with_disabled(&["kerminal.ssh", "kerminal.rdp"]));
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    assert!(
        !names
            .iter()
            .any(|name| name == "kerminal.vault.encrypt_secret"),
        "vault requires SSH or RDP: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|name| name == "kerminal.host.upsert_with_credential"),
        "host upsert stays visible for SFTP: {names:?}"
    );
    assert_disabled_units(
        &call(
            &client,
            "kerminal.vault.encrypt_secret",
            json!({
                "kind": "ssh-host",
                "hostId": "gated-host",
                "scope": "target",
                "material": "password",
                "plaintext": "test-secret"
            }),
        )
        .await,
        &["kerminal.ssh", "kerminal.rdp"],
        &["kerminal.ssh", "kerminal.rdp"],
    );
    // 默认新建 SSH 主机：目录可见，但请求级协议门禁仍拒绝已禁用的 SSH。
    assert_disabled_units(
        &call(
            &client,
            "kerminal.host.upsert_with_credential",
            json!({
                "name": "gated",
                "host": "example.test",
                "username": "demo",
                "password": "test-secret"
            }),
        )
        .await,
        &["kerminal.ssh"],
        &["kerminal.ssh"],
    );
    stop(client, server).await;

    let services = McpServices::new(Arc::new(FakeSessionService::default())).with_plugins(
        registry_with_disabled(&["kerminal.ssh", "kerminal.sftp", "kerminal.rdp"]),
    );
    let (client, server) = start_client(services).await;
    let names = tool_names(&client).await;
    for tool in [
        "kerminal.vault.encrypt_secret",
        "kerminal.host.upsert_with_credential",
    ] {
        assert!(!names.iter().any(|name| name == tool), "{tool}: {names:?}");
    }
    assert_disabled_units(
        &call(
            &client,
            "kerminal.vault.encrypt_secret",
            json!({
                "kind": "ssh-host",
                "hostId": "gated-host",
                "scope": "target",
                "material": "password",
                "plaintext": "test-secret"
            }),
        )
        .await,
        &["kerminal.ssh", "kerminal.rdp"],
        &["kerminal.ssh", "kerminal.rdp"],
    );
    assert_disabled_units(
        &call(
            &client,
            "kerminal.host.upsert_with_credential",
            json!({
                "name": "gated",
                "host": "example.test",
                "username": "demo",
                "password": "test-secret"
            }),
        )
        .await,
        &["kerminal.ssh", "kerminal.sftp", "kerminal.rdp"],
        &["kerminal.ssh", "kerminal.sftp", "kerminal.rdp"],
    );
    stop(client, server).await;

    // 仅 sftp 启用：host.upsert 可进入请求级；命中既有 SFTP 主机时不再被协议门禁拒绝。
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_connections(
            Arc::new(UnusedRemoteConnections),
            HostManagementService::new(Arc::new(FakeHostRepository {
                hosts: vec![sftp_host("gated-sftp-host")],
            })),
            CredentialService::new(Arc::new(UnusedVault)),
        )
        .with_plugins(registry_with_disabled(&["kerminal.ssh", "kerminal.rdp"]));
    let (client, server) = start_client(services).await;
    let sftp_upsert = call(
        &client,
        "kerminal.host.upsert_with_credential",
        json!({
            "name": "gated",
            "host": "example.test",
            "username": "demo",
            "password": "test-secret"
        }),
    )
    .await;
    assert_ne!(
        sftp_upsert["errorKind"], "plugin_disabled",
        "SFTP host reaches the request-level path: {sftp_upsert}"
    );
    stop(client, server).await;
}

/// Telnet 与串口会话的写入、快照和尺寸调整分别要求各自启停单元。
///
/// 即使 SSH 仍启用，也不能让已禁用协议的既有会话继续被 MCP 操作。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telnet_and_serial_sessions_require_their_own_units() {
    let sessions = Arc::new(FakeSessionService::default());
    let services = McpServices::new(sessions.clone())
        .with_plugins(registry_with_disabled(&["kerminal.telnet"]));
    let (client, server) = start_client(services).await;
    let telnet = sessions
        .seed(SessionTarget::Telnet(TelnetSessionTarget {
            host: "telnet.test".to_owned(),
            port: 23,
        }))
        .await;
    for (tool, args) in [
        (
            "terminal.write",
            json!({ "sessionId": telnet.as_str(), "data": "ls\n" }),
        ),
        ("terminal.snapshot", json!({ "sessionId": telnet.as_str() })),
        (
            "terminal.resize",
            json!({ "sessionId": telnet.as_str(), "cols": 100, "rows": 30 }),
        ),
    ] {
        assert_disabled_units(
            &call(&client, tool, args).await,
            &["kerminal.telnet"],
            &["kerminal.telnet"],
        );
    }
    stop(client, server).await;

    let sessions = Arc::new(FakeSessionService::default());
    let services = McpServices::new(sessions.clone())
        .with_plugins(registry_with_disabled(&["kerminal.serial"]));
    let (client, server) = start_client(services).await;
    let serial = sessions
        .seed(SessionTarget::Serial(SerialSessionTarget::new(
            PathBuf::from("COM3"),
        )))
        .await;
    for (tool, args) in [
        (
            "terminal.write",
            json!({ "sessionId": serial.as_str(), "data": "ls\n" }),
        ),
        ("terminal.snapshot", json!({ "sessionId": serial.as_str() })),
        (
            "terminal.resize",
            json!({ "sessionId": serial.as_str(), "cols": 100, "rows": 30 }),
        ),
    ] {
        assert_disabled_units(
            &call(&client, tool, args).await,
            &["kerminal.serial"],
            &["kerminal.serial"],
        );
    }
    stop(client, server).await;
}

/// 重试会按原请求重建入队任务，必须复用与 enqueue 相同的远端协议门禁。
///
/// 先在 SSH 启用时入队一个远端任务，再关闭 SSH 重试；拒绝必须发生在队列重建之前，
/// 且 plugin_disabled 指向 sftp.transfer.retry 而不是 enqueue。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_reuses_transfer_enqueue_target_gate() {
    let transfers = failing_transfer_service();
    let host_id = HostId::parse("gated-ssh-host").expect("valid host id");
    let queued = transfers
        .enqueue(TransferRequest {
            source: FileEndpoint::remote(host_id, "/tmp/source.bin").expect("valid remote source"),
            destination: FileEndpoint::Local {
                path: PathBuf::from("C:/tmp/destination.bin"),
            },
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Overwrite,
            idle_timeout_seconds: Some(60),
        })
        .await
        .expect("queued transfer is accepted");
    let host_repository = Arc::new(FakeHostRepository {
        hosts: vec![ssh_host("gated-ssh-host")],
    });
    let services = McpServices::new(Arc::new(FakeSessionService::default()))
        .with_connections(
            Arc::new(UnusedRemoteConnections),
            HostManagementService::new(host_repository),
            CredentialService::new(Arc::new(UnusedVault)),
        )
        .with_transfers(transfers.clone())
        .with_plugins(registry_with_disabled(&["kerminal.ssh"]));
    let (client, server) = start_client(services).await;
    let result = call(
        &client,
        "sftp.transfer.retry",
        json!({ "transferId": queued.id.as_str() }),
    )
    .await;
    assert_disabled_units(&result, &["kerminal.ssh"], &["kerminal.ssh"]);
    assert_eq!(result["data"]["toolId"], "sftp.transfer.retry", "{result}");
    stop(client, server).await;
    transfers
        .shutdown()
        .await
        .expect("transfer queue shuts down");
}
