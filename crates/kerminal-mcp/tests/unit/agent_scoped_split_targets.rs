//! Agent scoped endpoint 的同 Tab 双 pane 目标发现回归。
//!
//! 这里复用 MCP 的真实分派入口与严格显式 session 语义，但 application 端口由内存 fake 提供，
//! 因此只锁定 adapter 层的 tab/pane 投影、显式目标读取和“不回退”行为；真实 UI 分屏、
//! application 目标解析与 PTY 仍需独立的原生/组合根验证。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, sync::Arc};

use crate::{McpServices, agent_tools, terminal_tools};
use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    agents::{
        AgentHistoryProjection, AgentHistoryRequest, AgentLauncherCatalog, AgentResumeCapability,
        AgentService, AgentSessionContextPathsProjection, AgentSessionLaunchProjection,
        AgentSessionPathsProjection, AgentSessionRecordProjection,
        AgentSessionRecordSessionProjection, AgentSessionView,
        AgentTargetBindingContextBindingProjection, AgentTargetBindingContextProjection,
        AgentTargetContext, AgentTargetResolution, AgentTerminalSnapshotRequest, AgentTextPreview,
        AgentTextSubmissionReceipt, CreateAgentSessionFromHistoryRequest,
        CreateAgentSessionRequest, RenameAgentSessionTitleRequest, SendAgentCommandRequest,
    },
    plugins::PluginRegistry,
    preferences::{
        ConfigurationGuideStatus, ConfigurationRepository, PreferencesService, SettingsRepository,
        SettingsSnapshot, VersionedSettings,
    },
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionFilter, SessionRuntimeMetadata,
        SessionService, SessionSubscription, ShellCommandOutcome, ShellCommandRequest,
        TerminalInputEvent, TerminalPalette, TerminalSearchRequest, TerminalSearchResult,
        TerminalSelection, UiCommandLineRequest, UiSessionBinding,
    },
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionScope, AgentSessionStatus,
        AgentTargetBinding, AgentTargetLiveStatus,
    },
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState,
    },
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[path = "../support/commands_service.rs"]
mod commands_service;

/// 只服务本回归的内存会话表：真实实现快照/关闭语义，其余端口显式拒绝，避免伪成功。
#[derive(Default)]
struct SplitSessions {
    sessions: Mutex<BTreeMap<SessionId, FakeSession>>,
    writes: Mutex<Vec<(SessionId, Vec<u8>)>>,
}

struct FakeSession {
    info: SessionInfo,
    output: String,
}

impl SplitSessions {
    /// 预置一个同 Tab 的 UI pane；输出与 pane 身份一一对应，便于断言未串用目标。
    async fn seed_ui_pane(&self, tab_id: &TabId, pane_id: &PaneId, output: &str) -> SessionId {
        self.seed_session(
            SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            },
            output,
        )
        .await
    }

    /// 预置显式 headless 会话，验证后台模式仍允许后台 Agent 使用自己的 PTY。
    async fn seed_headless(&self, output: &str) -> SessionId {
        self.seed_session(SessionOwner::Mcp, output).await
    }

    /// 预置助手自身的 CLI 会话，确认后台模式不会把命令写回 Agent 进程。
    async fn seed_agent_cli(&self, agent_session_id: AgentSessionId, output: &str) -> SessionId {
        self.seed_session(SessionOwner::Agent { agent_session_id }, output)
            .await
    }

    /// 预置已退出的 headless 会话，确认残留显式目标给出 terminal.create 恢复。
    async fn seed_exited_headless(&self, output: &str) -> SessionId {
        self.seed_session_with_lifecycle(SessionOwner::Mcp, SessionLifecycle::Exited, output)
            .await
    }

    /// 统一登记内存会话，避免不同 owner 的 fixture 出现不一致的 target/size 默认值。
    async fn seed_session(&self, owner: SessionOwner, output: &str) -> SessionId {
        self.seed_session_with_lifecycle(owner, SessionLifecycle::Running, output)
            .await
    }

    /// 允许路由回归显式构造已退出会话，验证 owner 与 lifecycle 一起决定可写性。
    async fn seed_session_with_lifecycle(
        &self,
        owner: SessionOwner,
        lifecycle: SessionLifecycle,
        output: &str,
    ) -> SessionId {
        let session_id = SessionId::new();
        let info = SessionInfo {
            session_id: session_id.clone(),
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("split-fixture-shell"),
                args: Vec::new(),
                working_directory: Some(PathBuf::from("C:/fixture/split")),
                environment: BTreeMap::new(),
            }),
            owner,
            size: TerminalSize::default(),
            lifecycle,
            exit_status: None,
        };
        self.sessions.lock().await.insert(
            session_id.clone(),
            FakeSession {
                info,
                output: output.to_owned(),
            },
        );
        session_id
    }

    /// 返回已发生的写入，供断言拒绝发生在 writer 之前或 headless 写入确实到达。
    async fn writes(&self) -> Vec<(SessionId, Vec<u8>)> {
        self.writes.lock().await.clone()
    }
}

#[async_trait]
impl SessionService for SplitSessions {
    /// 本回归直接预置 UI 会话，不允许经 MCP create 伪造分屏来源。
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        unused_port()
    }

    /// 创建路径不属于同 Tab pane 发现回归的范围。
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test session create cancelled"));
        }
        unused_port()
    }

    /// 返回当前登记会话，供 target_context 与显式快照按真实 list 路径查找。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        Ok(self
            .sessions
            .lock()
            .await
            .values()
            .map(|session| session.info.clone())
            .collect())
    }

    /// 不伪造 PID/OSC 状态，只确认显式会话存在，避免运行时摘要影响本回归结论。
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

    /// 记录显式写入；允许测试区分“策略拒绝”与“写入已经到达终端”。
    async fn write(&self, session_id: SessionId, bytes: Vec<u8>) -> AppResult<()> {
        self.writes.lock().await.push((session_id, bytes));
        Ok(())
    }

    /// 本回归不模拟 GPUI 键盘编码。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本回归不执行可见 UI 命令行提交。
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<kerminal_application::sessions::TerminalCommandContext> {
        unused_port()
    }

    /// 本回归不调整终端网格。
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        unused_port()
    }

    /// 本回归不维护主题色盘。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本回归不维护 scrollback 上限。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        unused_port()
    }

    /// 本回归关注纯文本显式目标，不生成 GPUI 渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unused_port()
    }

    /// 返回指定 pane 自身的输出，并在显式 ID 失效时报错，绝不回退到另一个 pane。
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

    /// 本回归不维护滚动视口。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        unused_port()
    }

    /// 本回归不维护 UI 绑定滚动。
    async fn scroll_ui(
        &self,
        _binding: UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本回归不维护选区。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本回归没有选中文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        Ok(None)
    }

    /// 本回归不执行 scrollback 搜索。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unused_port()
    }

    /// 本回归不重连会话。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test reconnect cancelled"));
        }
        unused_port()
    }

    /// 只移除显式登记的会话；重复关闭必须返回 not_found，不能落到其它 pane。
    async fn close(&self, session_id: SessionId) -> AppResult<()> {
        self.sessions
            .lock()
            .await
            .remove(&session_id)
            .map(|_| ())
            .ok_or_else(|| AppError::not_found("session not found"))
    }

    /// 测试 fake 立即完成关闭，不维护独立 deadline。
    async fn close_until(
        &self,
        session_id: SessionId,
        _deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        self.close(session_id).await
    }

    /// 本回归不整体关闭终端 runtime。
    async fn close_all(&self) -> AppResult<()> {
        unused_port()
    }

    /// 本回归不订阅 UI 更新。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        unused_port()
    }

    /// 本回归不执行交互 shell 命令。
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test shell command cancelled"));
        }
        unused_port()
    }
}

/// 只实现本回归需要的 Agent 用例；目标读取始终来自预置绑定，给快照/关闭不会重绑它。
struct SplitAgent {
    view: AgentSessionView,
    context: AgentTargetContext,
    recorded: Mutex<Vec<SessionId>>,
    resolutions: Mutex<Vec<AgentSessionId>>,
}

impl SplitAgent {
    /// 保存一份显式首选 pane 的上下文，返回 clone 以免 MCP 调用反向改写预置绑定。
    fn new(view: AgentSessionView, context: AgentTargetContext) -> Self {
        Self {
            view,
            context,
            recorded: Mutex::new(Vec::new()),
            resolutions: Mutex::new(Vec::new()),
        }
    }

    /// 返回被 MCP 记录过快照的显式目标，用于确认读取非首选 pane 未替换首选绑定。
    async fn recorded_targets(&self) -> Vec<SessionId> {
        self.recorded.lock().await.clone()
    }

    /// 返回首选绑定解析次数，用于验证后台模式隐式写入在解析前就被拒绝。
    async fn resolution_count(&self) -> usize {
        self.resolutions.lock().await.len()
    }
}

#[async_trait]
impl AgentService for SplitAgent {
    /// 本回归不更新 workspace endpoint。
    async fn configure_mcp_endpoint(
        &self,
        _endpoint: Option<String>,
        _cancellation: CancellationToken,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本回归不读取 Launcher 目录。
    async fn launcher_catalog(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        unused_port()
    }

    /// 本回归不保存 Launcher 选择。
    async fn select_launcher(
        &self,
        _key: kerminal_domain::agents::AgentLauncherKey,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        unused_port()
    }

    /// 本回归不创建 Agent、终端或分屏。
    async fn create(
        &self,
        _request: CreateAgentSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_port()
    }

    /// 本回归只读取一个固定 Agent。
    async fn list(&self, _cancellation: CancellationToken) -> AppResult<Vec<AgentSessionView>> {
        unused_port()
    }

    /// 本回归不读取历史筛选投影。
    async fn history(
        &self,
        _request: AgentHistoryRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentHistoryProjection> {
        unused_port()
    }

    /// 本回归不从历史启动新会话。
    async fn create_from_history(
        &self,
        _request: CreateAgentSessionFromHistoryRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_port()
    }

    /// 本回归不改名会话。
    async fn rename_title(
        &self,
        _request: RenameAgentSessionTitleRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_port()
    }

    /// 返回固定 Active Agent，供 scoped endpoint 的存活复核通过。
    async fn get(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test Agent lookup cancelled"));
        }
        if agent_session_id != self.view.session.agent_session_id {
            return Err(AppError::not_found("test Agent not found"));
        }
        Ok(self.view.clone())
    }

    /// 本回归不恢复 CLI。
    async fn resume(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_port()
    }

    /// 本回归不归档 Agent。
    async fn archive(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_port()
    }

    /// 普通快照/关闭不得经由这里重绑首选目标。
    async fn bind_preferred_target(
        &self,
        _agent_session_id: AgentSessionId,
        _binding: AgentTargetBinding,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_port()
    }

    /// 返回预置首选绑定，供可见模式隐式写入走真实解析路径。
    async fn resolve_preferred_target(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetResolution> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "test Agent target resolution cancelled",
            ));
        }
        if agent_session_id != self.context.target.agent_session_id {
            return Err(AppError::not_found("test Agent not found"));
        }
        self.resolutions.lock().await.push(agent_session_id);
        Ok(self.context.target.clone())
    }

    /// 返回含两个 UI pane 的稳定上下文；每次读取都保留原首选绑定。
    async fn target_context(
        &self,
        agent_session_id: AgentSessionId,
        _max_snapshot_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetContext> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test Agent context cancelled"));
        }
        if agent_session_id != self.context.target.agent_session_id {
            return Err(AppError::not_found("test Agent not found"));
        }
        Ok(self.context.clone())
    }

    /// 本回归不生成文本预览。
    async fn preview_text(
        &self,
        _agent_session_id: AgentSessionId,
        _text: String,
        _max_bytes: usize,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTextPreview> {
        unused_port()
    }

    /// 本回归不向 Agent CLI 提交文本。
    async fn send_command_block_text(
        &self,
        _request: SendAgentCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTextSubmissionReceipt> {
        unused_port()
    }

    /// 记录显式读取的 pane 快照；这是观察行为，不能改变次选目标为首选绑定。
    async fn record_terminal_snapshot(
        &self,
        request: AgentTerminalSnapshotRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<()> {
        if request.agent_session_id != self.view.session.agent_session_id {
            return Err(AppError::not_found("test Agent not found"));
        }
        self.recorded.lock().await.push(request.target_session_id);
        Ok(())
    }
}

/// 构造可被 Agent 会话和 target_context 共同引用的 Agent CLI terminal。
fn agent_cli(agent_id: &AgentSessionId) -> SessionInfo {
    SessionInfo {
        session_id: SessionId::new(),
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("codex"),
            args: Vec::new(),
            working_directory: Some(PathBuf::from("C:/fixture/agent")),
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Agent {
            agent_session_id: agent_id.clone(),
        },
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    }
}

/// 构造仅含稳定绑定身份的 domain session，避免测试从 UI 焦点推导绑定。
fn agent_record(
    agent_id: &AgentSessionId,
    tab_id: &TabId,
    binding: &AgentTargetBinding,
) -> (AgentSession, AgentSessionRecordProjection) {
    let launch = AgentSessionLaunch {
        command_label: "codex".to_owned(),
        shell: "codex.exe".to_owned(),
        args: Vec::new(),
        cwd: "C:/fixture/agent".to_owned(),
    };
    let session = AgentSession::new(
        agent_id.clone(),
        AgentProvider::Codex,
        None,
        "Split target fixture".to_owned(),
        "1792200000000".to_owned(),
        "C:/fixture".to_owned(),
        "C:/fixture/agents/sessions/split".to_owned(),
        launch.clone(),
        Some(AgentSessionScope::Tab {
            tab_id: tab_id.clone(),
        }),
        Some(binding.clone()),
    )
    .expect("fixture Agent record must satisfy domain invariants");
    let record = AgentSessionRecordProjection {
        session: AgentSessionRecordSessionProjection {
            schema_version: 1,
            agent_session_id: agent_id.to_string(),
            agent_id: AgentProvider::Codex,
            launcher_key: Some("codex/default".to_owned()),
            title: session.title.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            status: AgentSessionStatus::Active,
            workspace_root: session.workspace_root.clone(),
            session_root: session.session_root.clone(),
            launch: AgentSessionLaunchProjection {
                command_label: launch.command_label,
                shell: launch.shell,
                args: launch.args,
                cwd: launch.cwd,
            },
            scope: session.scope.clone(),
            target: session.target.clone(),
        },
        provider: None,
        resume_recovery: None,
        target_binding: Some(AgentTargetBindingContextProjection {
            schema_version: 1,
            agent_session_id: agent_id.to_string(),
            scope: Some(AgentSessionScope::Tab {
                tab_id: tab_id.clone(),
            }),
            binding: AgentTargetBindingContextBindingProjection {
                binding_id: binding.binding_id.clone(),
                generation: binding.generation,
                status: "ready".to_owned(),
                stale: false,
                pane_id: binding.pane_id.as_ref().map(ToString::to_string),
                tab_id: binding.tab_id.as_ref().map(ToString::to_string),
                target_terminal_session_id: binding
                    .terminal_session_id
                    .as_ref()
                    .map(ToString::to_string),
                target_ref: binding.target_ref.clone(),
                cwd: binding.cwd.clone(),
                shell: binding.shell.clone(),
            },
            agent_terminal: None,
            generated_at: "1792200000001".to_owned(),
        }),
        mcp_endpoint: None,
        paths: AgentSessionPathsProjection {
            workspace_root: session.workspace_root.clone(),
            session_root: session.session_root.clone(),
            session_toml: "C:/fixture/agents/sessions/split/session.toml".to_owned(),
            provider_toml: "C:/fixture/agents/sessions/split/provider.toml".to_owned(),
            context: AgentSessionContextPathsProjection {
                target_binding_json: "C:/fixture/agents/sessions/split/context/target-binding.json"
                    .to_owned(),
                mcp_endpoint_json: "C:/fixture/agents/sessions/split/context/mcp-endpoint.json"
                    .to_owned(),
                terminal_snapshot_json:
                    "C:/fixture/agents/sessions/split/context/terminal-snapshot.json".to_owned(),
                workspace_snapshot_json:
                    "C:/fixture/agents/sessions/split/context/workspace-snapshot.json".to_owned(),
            },
        },
        diagnostics: Vec::new(),
    };
    (session, record)
}

/// 组装固定首选 pane 的上下文；快照文本对应首选 pane，避免与次选 pane 内容混淆。
fn target_context(
    agent_id: &AgentSessionId,
    session: AgentSession,
    record: AgentSessionRecordProjection,
    binding: AgentTargetBinding,
    preferred_session: SessionId,
    preferred_output: &str,
) -> AgentTargetContext {
    AgentTargetContext {
        session: AgentSessionView {
            session,
            terminal: None,
            resume_recovery: None,
            resume_capability: AgentResumeCapability::MetadataUnavailable,
        },
        record,
        target: AgentTargetResolution {
            agent_session_id: agent_id.clone(),
            binding,
            resolved_session_id: Some(preferred_session.clone()),
            status: AgentTargetLiveStatus::Ready,
            observed_at_ms: 1792200000002,
        },
        snapshot: Some(PlainTextSnapshot {
            session_id: preferred_session,
            text: preferred_output.to_owned(),
            first_line: 0,
            line_count: 1,
            total_scrollback_lines: 1,
            truncated: false,
        }),
        captured_bytes: preferred_output.len(),
        redacted: false,
    }
}

/// 把 JSON 参数转成工具分派的 Map，保持生产解析路径而不是直接构造响应。
fn args(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .cloned()
        .expect("tool arguments are objects")
}

/// 读取成功响应的 structured content；缺失即说明分派外形漂移。
fn success_data(result: &rmcp::model::CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("tool returns structured content")
}

/// 未涉及的应用端口必须显式失败，避免 fake 制造未验证的成功路径。
fn unused_port<T>() -> AppResult<T> {
    Err(AppError::unsupported("unused split target test port"))
}

/// 固定 Agent 命令路由设置，验证 MCP 每次请求读取已提交值而非启动快照。
struct RoutingSettingsRepository {
    enabled: bool,
    fail: bool,
}

#[async_trait]
impl SettingsRepository for RoutingSettingsRepository {
    /// 返回当前路由设置；fail 模拟真实配置不可读，确保调用方不能默认按可见终端放行。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        if self.fail {
            return Err(AppError::infrastructure("test settings unreadable"));
        }
        let mut settings = SettingsSnapshot::default();
        settings.terminal.agent_commands_in_terminal = self.enabled;
        Ok(VersionedSettings {
            settings,
            revision: None,
        })
    }

    /// 路由回归不写入设置，返回同一投影即可避免测试依赖磁盘 CAS。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        Ok(VersionedSettings {
            settings,
            revision: None,
        })
    }
}

#[async_trait]
impl ConfigurationRepository for RoutingSettingsRepository {
    /// 测试仓储恒为可写状态，隔离启动恢复逻辑。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 路由回归只走 typed 设置端口，不解析原始文档。
    async fn read(&self, _path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        Err(AppError::unsupported(
            "routing regression only uses typed settings",
        ))
    }

    /// 路由回归不模拟原始文件写入。
    async fn write(
        &self,
        _path: ConfigDocumentPath,
        _source: String,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported(
            "routing regression only uses typed settings",
        ))
    }

    /// 测试仓储不播种指南，保持当前版本状态。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationGuideStatus::Current
    }
}

/// 构造启用或关闭可见终端的设置服务。
fn routing_preferences(enabled: bool) -> PreferencesService {
    PreferencesService::new(Arc::new(RoutingSettingsRepository {
        enabled,
        fail: false,
    }))
}

/// 构造读取必然失败的设置服务，验证写入不会静默回落到可见终端。
fn unreadable_routing_preferences() -> PreferencesService {
    PreferencesService::new(Arc::new(RoutingSettingsRepository {
        enabled: true,
        fail: true,
    }))
}

/// 同 Tab UI 会话与首选绑定的公共 fixture；headless 会话用于验证后台显式写入。
struct RoutingFixture {
    sessions: Arc<SplitSessions>,
    agent: Arc<SplitAgent>,
    agent_id: AgentSessionId,
    ui_session: SessionId,
    headless_session: SessionId,
}

impl RoutingFixture {
    /// 复用 scoped endpoint 与真实分派入口装配服务，仅在需要时注入设置仓储。
    fn services(&self, preferences: Option<PreferencesService>, scoped: bool) -> McpServices {
        let mut services = McpServices::new(self.sessions.clone())
            .with_agents(self.agent.clone())
            .with_commands(commands_service::empty_commands_service(
                self.sessions.clone(),
            ));
        if let Some(preferences) = preferences {
            services = services.with_preferences(preferences);
        }
        if scoped {
            services = services.for_agent_session(self.agent_id.clone());
        }
        services
    }
}

/// 构造带首选 UI pane 与独立 headless 会话的 scoped Agent。
async fn routing_fixture() -> RoutingFixture {
    let agent_id = AgentSessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let sessions = Arc::new(SplitSessions::default());
    let ui_session = sessions
        .seed_ui_pane(&tab_id, &pane_id, "visible pane output")
        .await;
    let headless_session = sessions.seed_headless("headless output").await;

    let binding = AgentTargetBinding {
        binding_id: Some("binding-routing-3".to_owned()),
        generation: 3,
        pane_id: Some(pane_id),
        tab_id: Some(tab_id.clone()),
        terminal_session_id: Some(ui_session.clone()),
        target_ref: Some("local:routing".to_owned()),
        target_kind: Some("local".to_owned()),
        cwd: Some("C:/fixture/split".to_owned()),
        shell: Some("split-fixture-shell".to_owned()),
        live_status: AgentTargetLiveStatus::Ready,
        last_seen_at: Some("1792200000000".to_owned()),
    };
    let (session, record) = agent_record(&agent_id, &tab_id, &binding);
    let view = AgentSessionView {
        session: session.clone(),
        terminal: Some(agent_cli(&agent_id)),
        resume_recovery: None,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
    };
    let context = target_context(
        &agent_id,
        session,
        record,
        binding,
        ui_session.clone(),
        "visible pane output",
    );
    RoutingFixture {
        sessions,
        agent: Arc::new(SplitAgent::new(view, context)),
        agent_id,
        ui_session,
        headless_session,
    }
}

/// 同 Tab 两个 UI pane 可经 scoped target_context 发现，显式读取次选 pane 不会重绑首选目标。
///
/// 回归覆盖：terminals 中两个 pane 的 tabId/paneId 准确；显式 snapshot 只读取指定 pane；
/// target_context 仍返回原首选 pane；已关闭或不存在显式目标返回 not_found 且无回退。
/// 真实 application target_context 与 UI 分屏不在本 fixture 内，需另行原生验证。
///
/// @author kongweiguang
#[tokio::test]
async fn agent_scoped_context_discovers_split_panes_and_keeps_explicit_targets() {
    let agent_id = AgentSessionId::new();
    let tab_id = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let preferred_output = "preferred pane A output";
    let secondary_output = "secondary pane B output";

    let sessions = Arc::new(SplitSessions::default());
    let session_a = sessions
        .seed_ui_pane(&tab_id, &pane_a, preferred_output)
        .await;
    let session_b = sessions
        .seed_ui_pane(&tab_id, &pane_b, secondary_output)
        .await;

    let binding = AgentTargetBinding {
        binding_id: Some("binding-split-7".to_owned()),
        generation: 7,
        pane_id: Some(pane_a.clone()),
        tab_id: Some(tab_id.clone()),
        terminal_session_id: Some(session_a.clone()),
        target_ref: Some("local:split-a".to_owned()),
        target_kind: Some("local".to_owned()),
        cwd: Some("C:/fixture/split".to_owned()),
        shell: Some("split-fixture-shell".to_owned()),
        live_status: AgentTargetLiveStatus::Ready,
        last_seen_at: Some("1792200000000".to_owned()),
    };
    let (session, record) = agent_record(&agent_id, &tab_id, &binding);
    let view = AgentSessionView {
        session: session.clone(),
        terminal: Some(agent_cli(&agent_id)),
        resume_recovery: None,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
    };
    let context = target_context(
        &agent_id,
        session,
        record,
        binding,
        session_a.clone(),
        preferred_output,
    );
    let agent = Arc::new(SplitAgent::new(view, context));
    let services = McpServices::new(sessions.clone())
        .with_agents(agent.clone())
        .for_agent_session(agent_id.clone());

    let first = agent_tools::target_context(
        &services,
        args(json!({ "agentSessionId": agent_id.as_str() })),
        CancellationToken::new(),
    )
    .await;
    let first = success_data(&first);
    assert_eq!(
        first["nextHints"],
        json!([]),
        "fixtures without injected preferences keep the old empty hint contract"
    );
    assert_eq!(first["data"]["targetBinding"]["paneId"], pane_a.as_str());
    assert_eq!(
        first["data"]["targetBinding"]["targetTerminalSessionId"],
        session_a.as_str()
    );

    let terminals = first["data"]["terminals"]
        .as_array()
        .expect("target context exposes terminal list");
    assert_eq!(terminals.len(), 2, "both split panes stay discoverable");
    let pane_sessions = terminals
        .iter()
        .filter_map(|terminal| {
            Some((
                terminal["paneId"].as_str()?.to_owned(),
                terminal["sessionId"].as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        pane_sessions.get(pane_a.as_str()).map(String::as_str),
        Some(session_a.as_str())
    );
    assert_eq!(
        pane_sessions.get(pane_b.as_str()).map(String::as_str),
        Some(session_b.as_str())
    );
    for terminal in terminals {
        assert_eq!(
            terminal["tabId"],
            tab_id.as_str(),
            "both panes retain the same Tab identity"
        );
    }

    let snapshot_b = terminal_tools::snapshot(
        &services,
        args(json!({
            "sessionId": session_b.as_str(),
            "agentSessionId": agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let snapshot_b = success_data(&snapshot_b);
    assert_eq!(snapshot_b["data"]["snapshot"]["data"], secondary_output);
    assert_eq!(
        snapshot_b["data"]["session"]["sessionId"],
        session_b.as_str()
    );
    assert_eq!(snapshot_b["entities"][0]["id"], session_b.as_str());
    assert_eq!(
        agent.recorded_targets().await,
        vec![session_b.clone()],
        "explicit snapshot must record the secondary pane, not the preferred pane"
    );

    let after_read = agent_tools::target_context(
        &services,
        args(json!({ "agentSessionId": agent_id.as_str() })),
        CancellationToken::new(),
    )
    .await;
    let after_read = success_data(&after_read);
    assert_eq!(
        after_read["data"]["targetBinding"]["paneId"],
        pane_a.as_str()
    );
    assert_eq!(
        after_read["data"]["targetBinding"]["targetTerminalSessionId"],
        session_a.as_str(),
        "reading another pane must not rebind the Agent preferred target"
    );

    let closed = terminal_tools::close(
        &services,
        args(json!({
            "sessionId": session_b.as_str(),
            "agentSessionId": agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        success_data(&closed)["entities"][0]["id"],
        session_b.as_str()
    );

    let closed_snapshot = terminal_tools::snapshot(
        &services,
        args(json!({
            "sessionId": session_b.as_str(),
            "agentSessionId": agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let closed_snapshot = closed_snapshot
        .structured_content
        .expect("closed explicit target still returns a structured error");
    assert_eq!(closed_snapshot["errorKind"], "not_found");
    assert!(
        !closed_snapshot.to_string().contains(preferred_output),
        "a closed explicit target must not fall back to the preferred pane"
    );

    let missing = SessionId::new();
    let missing_snapshot = terminal_tools::snapshot(
        &services,
        args(json!({
            "sessionId": missing.as_str(),
            "agentSessionId": agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let missing_snapshot = missing_snapshot
        .structured_content
        .expect("unknown explicit target returns a structured error");
    assert_eq!(missing_snapshot["errorKind"], "not_found");
    assert!(
        !missing_snapshot.to_string().contains(preferred_output),
        "an unknown explicit target must not resolve to any known pane"
    );

    let missing_close = terminal_tools::close(
        &services,
        args(json!({
            "sessionId": missing.as_str(),
            "agentSessionId": agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        missing_close
            .structured_content
            .expect("unknown close returns a structured error")["errorKind"],
        "not_found"
    );

    let final_context = agent_tools::target_context(
        &services,
        args(json!({ "agentSessionId": agent_id.as_str() })),
        CancellationToken::new(),
    )
    .await;
    let final_context = success_data(&final_context);
    assert_eq!(
        final_context["data"]["targetBinding"]["paneId"],
        pane_a.as_str()
    );
    assert_eq!(
        final_context["data"]["targetBinding"]["targetTerminalSessionId"],
        session_a.as_str(),
        "failed explicit targets must not replace or clear the original binding"
    );
}

/// 从真实 HTTP 路径注入助手身份，防止后台策略只在手动设置 scope 的单测中生效。
///
/// @author kongweiguang
#[tokio::test]
async fn scoped_http_write_obeys_background_preference() {
    use rmcp::{
        ServiceExt,
        model::{CallToolRequestParams, ClientInfo},
        transport::streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    };

    let fixture = routing_fixture().await;
    let config = crate::McpServerConfig::new("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut server = crate::McpServer::start(
        &tokio::runtime::Handle::current(),
        fixture.services(Some(routing_preferences(false)), false),
        config,
    )
    .await
    .expect("启动隔离的回环 MCP 服务");
    let url = server.agent_endpoint(&fixture.agent_id);
    let client = ClientInfo::default()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url),
        ))
        .await
        .expect("通过助手专用地址连接 MCP");
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("terminal.write").with_arguments(args(json!({
                "sessionId": fixture.ui_session.as_str(),
                "data": "echo visible\n"
            }))),
        )
        .await
        .expect("收到结构化拒绝");
    assert_eq!(
        result.structured_content.unwrap()["data"]["route"],
        "background"
    );
    assert!(fixture.sessions.writes().await.is_empty());
    client.cancel().await.expect("关闭 MCP 客户端");
    server.shutdown().await.expect("关闭隔离的 MCP 服务");
}

/// 后台模式拒绝 scoped 可见终端写入，同时保留显式 headless、全局工具和只读上下文。
///
/// 回归覆盖：隐式写入不解析 preferred；显式 UI、Agent CLI 与已退出会话写到不了 writer；
/// 显式 Running MCP headless 仍可写入（包括设置不可读时）；全局调用即使携带
/// agentSessionId 也不受该开关影响；隐式写入在设置不可读时不放行。
///
/// @author kongweiguang
#[tokio::test]
async fn background_routing_rejects_visible_writes_and_keeps_background_paths() {
    let fixture = routing_fixture().await;
    let services = fixture.services(Some(routing_preferences(false)), true);

    let implicit = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo implicit\n",
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let implicit = implicit
        .structured_content
        .expect("background implicit write returns a structured error");
    assert_eq!(implicit["errorKind"], "conflict");
    assert_eq!(implicit["data"]["route"], "background");
    for expected in ["ssh.command", "terminal.create"] {
        assert!(
            implicit["summary"].to_string().contains(expected),
            "rejection must point at {expected}"
        );
    }
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "implicit rejection must happen before the writer"
    );
    assert_eq!(
        fixture.agent.resolution_count().await,
        0,
        "implicit rejection must happen before preferred target resolution"
    );

    let ui = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo ui\n",
            "sessionId": fixture.ui_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        ui.structured_content
            .expect("background UI write returns a structured error")["errorKind"],
        "conflict"
    );
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "explicit UI rejection must happen before the writer"
    );

    // 显式 headless id 已不存在时也必须在 writer 前失败，并提示重新 create，
    // 不能把 application 的通用 not-found 当成可以回退到可见终端的信号。
    let missing_session = SessionId::new();
    let missing = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo gone\n",
            "sessionId": missing_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let missing = missing
        .structured_content
        .expect("missing background session returns a structured error");
    assert_eq!(missing["errorKind"], "conflict");
    assert_eq!(missing["data"]["fallback"], "terminal.create");
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "missing background session must fail before the writer"
    );

    // Agent 自身的 CLI 与 UI 一样不是后台命令落点；只有 terminal.create 的 MCP 会话可写。
    let agent_cli_session = fixture
        .sessions
        .seed_agent_cli(fixture.agent_id.clone(), "agent cli output")
        .await;
    let agent_cli = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo agent\n",
            "sessionId": agent_cli_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let agent_cli = agent_cli
        .structured_content
        .expect("background Agent CLI write returns a structured error");
    assert_eq!(agent_cli["errorKind"], "conflict");
    assert_eq!(agent_cli["data"]["route"], "background");
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "Agent CLI rejection must happen before the writer"
    );

    // Service 会话虽然也是 headless，但属于内部编排目标；后台模式只接受 MCP owner。
    let service_session = fixture
        .sessions
        .seed_session(
            SessionOwner::Service {
                name: "split-fixture-service".to_owned(),
            },
            "service output",
        )
        .await;
    let service = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo service\n",
            "sessionId": service_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let service = service
        .structured_content
        .expect("background service session write returns a structured error");
    assert_eq!(service["errorKind"], "conflict");
    assert_eq!(service["data"]["route"], "background");
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "internal service sessions are not background command targets"
    );

    // 已退出的显式 headless 也不能继续写入；残留目标是重新 create 的信号。
    let exited_session = fixture
        .sessions
        .seed_exited_headless("exited headless output")
        .await;
    let exited = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo exited\n",
            "sessionId": exited_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let exited = exited
        .structured_content
        .expect("exited background session returns a structured error");
    assert_eq!(exited["errorKind"], "conflict");
    assert_eq!(exited["data"]["fallback"], "terminal.create");
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "exited background session must fail before the writer"
    );

    // 全局 endpoint 即使显式携带 agentSessionId，也不属于右侧栏 scoped 路由。
    let global = fixture.services(Some(routing_preferences(false)), false);
    let global_write = terminal_tools::write(
        &global,
        args(json!({
            "data": "echo external\n",
            "sessionId": fixture.ui_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    success_data(&global_write);

    let headless = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo headless\n",
            "sessionId": fixture.headless_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    success_data(&headless);
    let writes = fixture.sessions.writes().await;
    assert_eq!(
        writes.len(),
        2,
        "global and explicit headless writes execute"
    );
    assert_eq!(writes[0].0, fixture.ui_session);
    assert_eq!(writes[1].0, fixture.headless_session);

    let context = agent_tools::target_context(
        &services,
        args(json!({ "agentSessionId": fixture.agent_id.as_str() })),
        CancellationToken::new(),
    )
    .await;
    let hints = success_data(&context)["nextHints"].to_string();
    assert!(
        hints.contains("后台执行") && hints.contains("terminal.create"),
        "background hint must name background execution and local headless creation"
    );
    assert!(
        hints.contains("ssh.command"),
        "background hint must name the non-interactive SSH path"
    );

    let unreadable = fixture.services(Some(unreadable_routing_preferences()), true);
    let blocked = terminal_tools::write(
        &unreadable,
        args(json!({
            "data": "echo blocked\n",
            "sessionId": fixture.ui_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        blocked
            .structured_content
            .expect("unreadable settings returns a structured error")["errorKind"],
        "infrastructure"
    );
    assert_eq!(
        fixture.sessions.writes().await.len(),
        2,
        "unreadable settings must not silently fall back to visible mode"
    );

    // 显式 Running MCP headless 已由 owner/lifecycle 确认为安全目标，
    // 不读取路由设置也应继续执行，避免设置故障切断后台恢复路径。
    let unreadable_headless = terminal_tools::write(
        &unreadable,
        args(json!({
            "data": "echo unreadable headless\n",
            "sessionId": fixture.headless_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    success_data(&unreadable_headless);
    assert_eq!(
        fixture.sessions.writes().await.len(),
        3,
        "explicit running MCP headless stays writable when settings are unreadable"
    );

    // 隐式写入无法确认目标是否可见，设置不可读时必须失败，不能解析首选绑定。
    let unreadable_implicit = terminal_tools::write(
        &unreadable,
        args(json!({
            "data": "echo unreadable implicit\n",
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        unreadable_implicit
            .structured_content
            .expect("unreadable implicit write returns a structured error")["errorKind"],
        "infrastructure"
    );
    assert_eq!(
        fixture.agent.resolution_count().await,
        0,
        "unreadable implicit write must fail before preferred target resolution"
    );
    assert_eq!(
        fixture.sessions.writes().await.len(),
        3,
        "unreadable implicit write must not reach the writer"
    );
    let unreadable_context = agent_tools::target_context(
        &unreadable,
        args(json!({ "agentSessionId": fixture.agent_id.as_str() })),
        CancellationToken::new(),
    )
    .await;
    assert!(
        success_data(&unreadable_context)["nextHints"]
            .to_string()
            .contains("无法读取 Agent 命令路由设置"),
        "context read must surface the unreadable setting instead of pretending visible mode"
    );
}

/// 未注入设置仓储时 scoped 写入不能默认按可见终端放行。
///
/// 显式 Running MCP headless 是唯一不依赖路由设置的写入路径；离线 fixture 的
/// 只读上下文仍保持原有空提示契约，避免为了补设置仓储扩大测试装配。
///
/// @author kongweiguang
#[tokio::test]
async fn missing_routing_settings_fail_closed_except_explicit_mcp_headless() {
    let fixture = routing_fixture().await;
    let services = fixture.services(None, true);

    let implicit = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo implicit missing settings\n",
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let implicit = implicit
        .structured_content
        .expect("missing settings implicit write returns a structured error");
    assert_eq!(implicit["errorKind"], "unsupported");
    assert_eq!(implicit["data"]["route"], "unavailable");
    assert_eq!(implicit["data"]["fallback"], "terminal.create");
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "missing settings must not fall back to the visible preferred target"
    );
    assert_eq!(
        fixture.agent.resolution_count().await,
        0,
        "missing settings must fail before preferred target resolution"
    );

    let ui = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo ui missing settings\n",
            "sessionId": fixture.ui_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        ui.structured_content
            .expect("missing settings UI write returns a structured error")["errorKind"],
        "unsupported"
    );
    assert!(
        fixture.sessions.writes().await.is_empty(),
        "missing settings must not allow an explicit visible target"
    );

    let headless = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo headless without settings\n",
            "sessionId": fixture.headless_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    success_data(&headless);
    assert_eq!(
        fixture.sessions.writes().await,
        vec![(
            fixture.headless_session.clone(),
            b"echo headless without settings\n".to_vec()
        )],
        "explicit running MCP headless does not depend on routing settings"
    );

    assert!(
        terminal_tools::agent_routing_hints(&services, true)
            .await
            .is_empty(),
        "fixtures without injected preferences keep the old empty hint contract"
    );
}

/// 关闭 SSH 插件时后台提示不应再引导 ssh.command，terminal.create 路径保持可用。
///
/// @author kongweiguang
#[tokio::test]
async fn background_hint_follows_ssh_command_visibility_gate() {
    let fixture = routing_fixture().await;
    let services = fixture
        .services(Some(routing_preferences(false)), true)
        .with_plugins(Arc::new(PluginRegistry::empty()));
    let hints = terminal_tools::agent_routing_hints(&services, true)
        .await
        .join(" ");
    assert!(
        hints.contains("terminal.create"),
        "local headless creation must remain the primary recovery path"
    );
    assert!(
        !hints.contains("ssh.command"),
        "hidden ssh.command must not be suggested by live hints"
    );

    let rejection = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo hidden ssh\n",
            "sessionId": fixture.ui_session.as_str(),
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    let summary = rejection
        .structured_content
        .expect("hidden SSH background write returns a structured error")["summary"]
        .to_string();
    assert!(summary.contains("terminal.create"));
    assert!(
        !summary.contains("ssh.command"),
        "background rejection must not point at a hidden ssh.command tool"
    );
}

/// 可见模式保持首选目标解析与写入，并在 scoped target_context 中提示绑定的主终端 Tab。
///
/// @author kongweiguang
#[tokio::test]
async fn visible_routing_keeps_preferred_write_and_exposes_live_hint() {
    let fixture = routing_fixture().await;
    let services = fixture.services(Some(routing_preferences(true)), true);

    let write = terminal_tools::write(
        &services,
        args(json!({
            "data": "echo visible\n",
            "agentSessionId": fixture.agent_id.as_str()
        })),
        CancellationToken::new(),
    )
    .await;
    success_data(&write);
    let writes = fixture.sessions.writes().await;
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].0, fixture.ui_session,
        "visible routing still targets the preferred UI terminal"
    );
    assert_eq!(fixture.agent.resolution_count().await, 1);

    let context = agent_tools::target_context(
        &services,
        args(json!({ "agentSessionId": fixture.agent_id.as_str() })),
        CancellationToken::new(),
    )
    .await;
    let hints = success_data(&context)["nextHints"].to_string();
    assert!(
        hints.contains("绑定的主终端 Tab") && hints.contains("terminal.write"),
        "visible hint must name the bound main terminal Tab and write tool"
    );
    let unavailable = terminal_tools::agent_routing_hints(&services, false).await;
    assert!(unavailable[0].contains("没有可用的首选终端"));
    assert!(!unavailable[0].contains("terminal.write"));
}
