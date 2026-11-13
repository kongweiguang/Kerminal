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
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 只服务本回归的内存会话表：真实实现快照/关闭语义，其余端口显式拒绝，避免伪成功。
#[derive(Default)]
struct SplitSessions {
    sessions: Mutex<BTreeMap<SessionId, FakeSession>>,
}

struct FakeSession {
    info: SessionInfo,
    output: String,
}

impl SplitSessions {
    /// 预置一个同 Tab 的 UI pane；输出与 pane 身份一一对应，便于断言未串用目标。
    async fn seed_ui_pane(&self, tab_id: &TabId, pane_id: &PaneId, output: &str) -> SessionId {
        let session_id = SessionId::new();
        let info = SessionInfo {
            session_id: session_id.clone(),
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("split-fixture-shell"),
                args: Vec::new(),
                working_directory: Some(PathBuf::from("C:/fixture/split")),
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            },
            size: TerminalSize::default(),
            lifecycle: SessionLifecycle::Running,
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

    /// 本回归不写入终端输入。
    async fn write(&self, _session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        unused_port()
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
}

impl SplitAgent {
    /// 保存一份显式首选 pane 的上下文，返回 clone 以免 MCP 调用反向改写预置绑定。
    fn new(view: AgentSessionView, context: AgentTargetContext) -> Self {
        Self {
            view,
            context,
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// 返回被 MCP 记录过快照的显式目标，用于确认读取非首选 pane 未替换首选绑定。
    async fn recorded_targets(&self) -> Vec<SessionId> {
        self.recorded.lock().await.clone()
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

    /// 本回归不通过省略 sessionId 的路径解析目标。
    async fn resolve_preferred_target(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTargetResolution> {
        unused_port()
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
