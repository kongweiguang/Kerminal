//! 验证 UI 建议上下文只依据 owner、真实 OSC 133、终端模式和输入来源。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::AppResult;
use kerminal_application::sessions::{
    CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
    RenderSnapshotRequest, ScrollRequest, SessionFilter, SessionService, SessionTransport,
    SessionTransportControl, SessionTransportFactory, SessionTransportOpenError,
    ShellCommandOutcome, ShellCommandRequest, ShellCommandUnknownReason,
    TerminalCommandAvailability, TerminalCommandUnavailableReason, TerminalSearchRequest,
    UiCommandLineRequest, UiInputProjection, UiSessionBinding, UiSnippetCommandLineRequest,
    UiTerminalInputRequest, UiTerminalSearchCommitGate, UiTerminalSearchRequest,
    UiTerminalSearchResult, UiTerminalSearchViewport,
};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::identity::{AgentSessionId, PaneId, SessionId, TabId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionLifecycle, SessionOwner, SessionTarget,
    TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct ContextFactory {
    output: Mutex<Option<DuplexStream>>,
    dialect: RemoteShellDialect,
}

#[async_trait]
impl SessionTransportFactory for ContextFactory {
    /// 使用 duplex transport 控制实际 PTY 输出，测试仍经过生产 engine 与 SessionManager reader。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let (output, server) = tokio::io::duplex(4096);
        *self.output.lock().await = Some(output);
        let (reader, writer) = tokio::io::split(server);
        Ok(SessionTransport::new(reader, writer, ContextControl)
            .with_shell_dialect(self.dialect)
            .with_shell_integration_enabled(true))
    }
}

struct ContextControl;

#[async_trait]
impl SessionTransportControl for ContextControl {
    /// 测试网格调整只改变 emulator，不持有外部 PTY 资源。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 测试 transport 关闭可重复调用，配合 runtime 的真实 reader 取消路径。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        Ok(())
    }

    /// 返回确定的测试退出状态，不把成功写入误当成 shell 命令完成。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

struct FailedOutputReader {
    bytes: Vec<u8>,
    offset: usize,
}

impl AsyncRead for FailedOutputReader {
    /// 先完整交付测试历史，再以真实 reader 错误进入 Failed，验证失败会话仍可浏览网格。
    ///
    /// @author kongweiguang
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.offset == self.bytes.len() {
            return Poll::Ready(Err(io::Error::other("test reader failure after history")));
        }

        let count = output
            .remaining()
            .min(self.bytes.len().saturating_sub(self.offset));
        output.put_slice(&self.bytes[self.offset..self.offset + count]);
        self.offset += count;
        Poll::Ready(Ok(()))
    }
}

struct FailedOutputFactory {
    bytes: Vec<u8>,
}

#[async_trait]
impl SessionTransportFactory for FailedOutputFactory {
    /// 用可控 reader 错误构造同一有界 SessionManager 数据路径，不伪造 lifecycle DTO。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        Ok(SessionTransport::new(
            FailedOutputReader {
                bytes: self.bytes.clone(),
                offset: 0,
            },
            tokio::io::sink(),
            ContextControl,
        )
        .with_shell_dialect(RemoteShellDialect::Posix)
        .with_shell_integration_enabled(true))
    }
}

struct GatedCloseFactory {
    output: Mutex<Option<DuplexStream>>,
    close_entered: Arc<Notify>,
    close_release: Arc<Semaphore>,
}

struct GatedCloseControl {
    close_entered: Arc<Notify>,
    close_release: Arc<Semaphore>,
}

#[async_trait]
impl SessionTransportFactory for GatedCloseFactory {
    /// 在真正关闭 transport 前设置可控栅栏，使测试能验证 Closing 状态下无滚动副作用。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let (output, server) = tokio::io::duplex(4096);
        *self.output.lock().await = Some(output);
        let (reader, writer) = tokio::io::split(server);
        Ok(SessionTransport::new(
            reader,
            writer,
            GatedCloseControl {
                close_entered: Arc::clone(&self.close_entered),
                close_release: Arc::clone(&self.close_release),
            },
        ))
    }
}

#[async_trait]
impl SessionTransportControl for GatedCloseControl {
    /// 测试 resize 不接触原生 PTY，但仍实现真实 transport control 契约。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 把 close 暂停在 transport 边界，供状态门禁测试在 Closing 阶段执行只读滚动。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: Instant) -> AppResult<()> {
        self.close_entered.notify_one();
        let permit =
            tokio::time::timeout_at(deadline, Arc::clone(&self.close_release).acquire_owned())
                .await
                .map_err(|_| kerminal_application::AppError::timeout("关闭栅栏测试等待超时"))?
                .map_err(|_| kerminal_application::AppError::infrastructure("关闭栅栏已关闭"))?;
        permit.forget();
        Ok(())
    }

    /// 返回确定测试状态，关闭栅栏放行后由 SessionManager 完成 reader 回收。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

struct TriggeredEofReader {
    trigger: tokio::sync::oneshot::Receiver<()>,
    ended: bool,
}

impl AsyncRead for TriggeredEofReader {
    /// 将 EOF 延后到 session 发布 Running 后，避免测试依赖 spawn 调度顺序。
    ///
    /// @author kongweiguang
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        _output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.ended {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut self.trigger).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => {
                self.ended = true;
                Poll::Ready(Ok(()))
            }
        }
    }
}

struct TriggeredEofFactory {
    output_peer: Mutex<Option<DuplexStream>>,
    eof_triggers: Mutex<Vec<tokio::sync::oneshot::Sender<()>>>,
}

impl TriggeredEofFactory {
    /// 让下一代 reader 在 create/reconnect 已发布后自然 EOF，保留 writer peer 检查滚动不写入。
    ///
    /// @author kongweiguang
    async fn finish_current_reader(&self) {
        if let Some(trigger) = self.eof_triggers.lock().await.pop() {
            let _ = trigger.send(());
        }
    }
}

#[async_trait]
impl SessionTransportFactory for TriggeredEofFactory {
    /// 提供独立 reader EOF 与仍可观察的 writer，精确验证 Exited 视图滚动不会发送字节。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let (output_peer, server) = tokio::io::duplex(4096);
        *self.output_peer.lock().await = Some(output_peer);
        let (_reader, writer) = tokio::io::split(server);
        let (trigger, receiver) = tokio::sync::oneshot::channel();
        self.eof_triggers.lock().await.push(trigger);
        Ok(SessionTransport::new(
            TriggeredEofReader {
                trigger: receiver,
                ended: false,
            },
            writer,
            ContextControl,
        ))
    }
}

/// 创建带显式 UI owner 的 session，并将原始输出端留给测试控制 OSC/DEC 事件。
///
/// @author kongweiguang
async fn create_ui_session(
    dialect: RemoteShellDialect,
) -> (Arc<SessionManager>, Arc<ContextFactory>, SessionId) {
    let factory = Arc::new(ContextFactory {
        output: Mutex::new(None),
        dialect,
    });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid terminal size"),
            initial_palette: None,
        })
        .await
        .expect("create test session");
    (manager, factory, info.session_id)
}

/// 创建一个可见 Agent pane 所属的 PTY，验证它与 UI 共用 owner/generation 输入门禁。
///
/// @author kongweiguang
async fn create_agent_session(
    dialect: RemoteShellDialect,
) -> (Arc<SessionManager>, Arc<ContextFactory>, SessionId) {
    let factory = Arc::new(ContextFactory {
        output: Mutex::new(None),
        dialect,
    });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Agent {
                agent_session_id: AgentSessionId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid terminal size"),
            initial_palette: None,
        })
        .await
        .expect("create Agent-owned test session");
    (manager, factory, info.session_id)
}

/// 等待 reader 完成 OSC 133 消费后再断言，避免测试依赖调度时序。
///
/// @author kongweiguang
async fn wait_for_prompt(
    service: &dyn SessionService,
    session_id: &SessionId,
) -> kerminal_application::sessions::TerminalCommandContext {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let context = service
                .terminal_command_context(session_id.clone())
                .await
                .expect("query terminal context");
            if context.prompt_ready {
                return context;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("shell prompt markers processed")
}

/// 等待 prompt epoch 前进且新一轮 A/B 均已消费，避免复用上一轮仍 ready 的状态。
///
/// @author kongweiguang
async fn wait_for_prompt_after(
    service: &dyn SessionService,
    session_id: &SessionId,
    after_epoch: u64,
) -> kerminal_application::sessions::TerminalCommandContext {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let context = service
                .terminal_command_context(session_id.clone())
                .await
                .expect("query terminal context");
            if context.prompt_epoch > after_epoch && context.prompt_ready {
                return context;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("next shell prompt markers processed")
}

/// 等待写入意图登记而不依赖 writer 背压触发的调度顺序。
///
/// @author kongweiguang
async fn wait_for_input_generation(
    service: &dyn SessionService,
    session_id: &SessionId,
    minimum: u64,
) -> kerminal_application::sessions::TerminalCommandContext {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let context = service
                .terminal_command_context(session_id.clone())
                .await
                .expect("query terminal context");
            if context.input_generation >= minimum {
                return context;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("input intent registered")
}

/// 等待测试输出来到可见网格，避免滚动断言依赖 reader task 的调度时序。
///
/// @author kongweiguang
async fn wait_for_render_marker(
    service: &dyn SessionService,
    session_id: &SessionId,
    marker: &str,
) -> RenderSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = service
                .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("render terminal grid");
            if snapshot
                .rows
                .iter()
                .any(|row| row.runs.iter().any(|run| run.text.contains(marker)))
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal output reaches visible grid")
}

/// 等待 PTY reader 把标记写入 bounded history，避免当前视口停留在旧位置时误判输出未到达。
///
/// @author kongweiguang
async fn wait_for_history_marker(
    service: &dyn SessionService,
    session_id: &SessionId,
    marker: &str,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let history = service
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 256,
                        max_bytes: Some(64 * 1024),
                    },
                )
                .await
                .expect("read bounded terminal history");
            if history.text.contains(marker) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal output reaches bounded history");
}

/// 等待真实 reader EOF 推进 lifecycle；不会把 close 请求本身当作自然退出。
/// 用 let-chain 保持身份与状态双重匹配，轮询条件不改变。
///
/// @author kongweiguang
async fn wait_for_lifecycle(
    service: &dyn SessionService,
    session_id: &SessionId,
    expected: SessionLifecycle,
) -> kerminal_domain::sessions::SessionInfo {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(info) = service
                .list(SessionFilter::default())
                .await
                .expect("list terminal sessions")
                .into_iter()
                .find(|info| &info.session_id == session_id)
                && info.lifecycle == expected
            {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal session reaches expected lifecycle")
}

/// 构造新的 UI 搜索意图，并将目标命中索引与正反向查询参数分开。
///
/// @author kongweiguang
fn ui_search_request(query: &str, selected_match: Option<usize>) -> UiTerminalSearchRequest {
    ui_search_request_with_viewport(
        query,
        selected_match,
        UiTerminalSearchViewport::Reveal { safe_top_rows: 0 },
    )
}

/// 构造带显式视口意图和一次性提交门的 UI 搜索请求，避免测试默认混淆预览和导航。
///
/// @author kongweiguang
fn ui_search_request_with_viewport(
    query: &str,
    selected_match: Option<usize>,
    viewport: UiTerminalSearchViewport,
) -> UiTerminalSearchRequest {
    UiTerminalSearchRequest {
        request: TerminalSearchRequest {
            query: query.to_owned(),
            case_sensitive: false,
            regex: false,
            backwards: false,
            max_matches: 256,
        },
        selected_match,
        viewport,
        commit_gate: UiTerminalSearchCommitGate::new(),
    }
}

/// 从当前绘制快照读取某个 engine line 的文本，验证搜索坐标指向的确是新命中。
///
/// @author kongweiguang
fn rendered_row_text(snapshot: &RenderSnapshot, line: i64) -> Option<String> {
    snapshot
        .rows
        .iter()
        .find(|row| row.absolute_line == line)
        .map(|row| {
            row.runs
                .iter()
                .map(|run| run.text.as_ref())
                .collect::<String>()
        })
}

/// 将 search 返回的 engine line 映射到包含 scrollback 的纯文本快照，避免把历史结果误当作同屏行。
///
/// @author kongweiguang
fn historical_row_text(snapshot: &PlainTextSnapshot, line: i64) -> Option<&str> {
    let row_index = usize::try_from(line.checked_sub(snapshot.first_line)?).ok()?;
    if row_index >= snapshot.line_count {
        return None;
    }
    snapshot.text.split('\n').nth(row_index)
}

/// 普通命令提交必须复核可见 owner generation，并在 shell marker 未就绪时仍写入一次粘贴加 Enter。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_command_line_submission_uses_exact_binding_without_shell_readiness() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let before = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("initial terminal context");
    assert_eq!(
        before.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::ShellIntegrationNotObserved
        )
    );

    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: before.owner.clone(),
        session_generation: before.session_generation,
    };
    let stale = manager
        .submit_ui_command_line(UiCommandLineRequest {
            binding: UiSessionBinding {
                session_id: binding.session_id.clone(),
                owner: binding.owner.clone(),
                session_generation: binding.session_generation.wrapping_add(1),
            },
            command: "must-not-be-written".to_owned(),
        })
        .await
        .expect_err("stale view generations must be rejected before the writer");
    assert_eq!(
        stale.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    let after = manager
        .submit_ui_command_line(UiCommandLineRequest {
            binding,
            command: "echo visible".to_owned(),
        })
        .await
        .expect("valid visible UI binding writes without shell readiness");
    assert_eq!(
        after.input_generation,
        before.input_generation.wrapping_add(1)
    );

    let expected = b"echo visible\r";
    let mut written = vec![0; expected.len()];
    tokio::time::timeout(Duration::from_secs(2), output.read_exact(&mut written))
        .await
        .expect("writer acknowledged command bytes")
        .expect("read combined paste and Enter bytes");
    assert_eq!(written, expected);

    manager.close_all().await.expect("close test session");
}

/// 片段专用事务保留 generation CAS，却允许 Unknown shell 与普通 paste 模式的多行正文直接写入。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_snippet_submission_writes_multiline_with_unknown_shell_without_prompt_gate() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Unknown).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let before = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("initial terminal context");
    assert_eq!(before.shell_dialect, RemoteShellDialect::Unknown);
    assert!(!before.prompt_ready);
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: before.owner.clone(),
        session_generation: before.session_generation,
    };

    let ordinary_multiline = manager
        .submit_ui_command_line(UiCommandLineRequest {
            binding: binding.clone(),
            command: "echo first\necho second".to_owned(),
        })
        .await
        .expect_err("ordinary command-line submissions retain their safety gate");
    assert_eq!(
        ordinary_multiline.public_code(),
        kerminal_application::PublicErrorCode::Unsupported
    );

    let stale = manager
        .submit_ui_snippet_command_line(UiSnippetCommandLineRequest {
            binding: UiSessionBinding {
                session_id: binding.session_id.clone(),
                owner: binding.owner.clone(),
                session_generation: binding.session_generation.wrapping_add(1),
            },
            command: "must-not-be-written".to_owned(),
        })
        .await
        .expect_err("stale snippet views remain fenced by the writer generation");
    assert_eq!(
        stale.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    manager
        .submit_ui_snippet_command_line(UiSnippetCommandLineRequest {
            binding,
            command: "echo first\necho second".to_owned(),
        })
        .await
        .expect("direct snippet sends do not require a known shell or ready prompt");

    let expected = b"echo first\recho second\r";
    let mut written = vec![0; expected.len()];
    tokio::time::timeout(Duration::from_secs(2), output.read_exact(&mut written))
        .await
        .expect("writer acknowledged all snippet bytes")
        .expect("read paste and Enter bytes");
    assert_eq!(written, expected);

    manager.close_all().await.expect("close test session");
}

/// 未见 A/B 时不显示建议；UI 输入保留 typed provenance，raw Agent/MCP 输入使其失效。
/// 分阶段核对 writer 的精确字节，避免把上一轮未消费输入误认作本次 composition 的 ACK。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_context_tracks_prompt_owner_generations_and_raw_input() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let initial = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("initial context");
    assert_eq!(
        initial.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::ShellIntegrationNotObserved
        )
    );

    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send prompt markers through test transport");
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    assert_eq!(prompt.availability, TerminalCommandAvailability::Available);
    assert!(prompt.prompt_line_empty);

    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: prompt.owner.clone(),
        session_generation: prompt.session_generation,
    };
    let after_ui = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: Some(prompt.version()),
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Text("pwd".to_owned()),
        })
        .await
        .expect("version-matched UI input");
    assert_eq!(
        after_ui.availability,
        TerminalCommandAvailability::Available
    );
    assert!(!after_ui.prompt_line_empty);
    assert_eq!(
        after_ui.input_generation,
        prompt.input_generation.wrapping_add(1)
    );

    let stale = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: Some(prompt.version()),
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Text(" stale".to_owned()),
        })
        .await
        .expect_err("old ghost text version must not be accepted");
    assert_eq!(
        stale.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    let submitted = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: Some(after_ui.version()),
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Key(TerminalKeyEvent {
                key: TerminalKey::Enter,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            }),
        })
        .await
        .expect("tracked UI command submission");
    assert_eq!(
        submitted.availability,
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady)
    );
    assert!(!submitted.prompt_line_empty);

    manager
        .write(session_id.clone(), b"agent-input".to_vec())
        .await
        .expect("raw external input");
    let after_external = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("context after raw input");
    assert_eq!(
        after_external.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::ExternalInputObserved
        )
    );

    let mut preceding_input = [0_u8; 15];
    tokio::time::timeout(
        Duration::from_secs(2),
        output.read_exact(&mut preceding_input),
    )
    .await
    .expect("prior input bytes were acknowledged")
    .expect("consume only the preceding writes");
    assert_eq!(&preceding_input, b"pwd\ragent-input");

    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send next prompt markers");
    let next_prompt =
        wait_for_prompt_after(manager.as_ref(), &session_id, prompt.prompt_epoch).await;
    assert_eq!(
        next_prompt.availability,
        TerminalCommandAvailability::Available
    );
    assert!(next_prompt.prompt_epoch > prompt.prompt_epoch);

    let composition_commit = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: UiSessionBinding {
                session_id: session_id.clone(),
                owner: next_prompt.owner.clone(),
                session_generation: next_prompt.session_generation,
            },
            expected_context: None,
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Text("composition".to_owned()),
        })
        .await
        .expect("Root-confirmed committed Text uses the exact visible binding and shared writer");
    assert_eq!(
        composition_commit.availability,
        TerminalCommandAvailability::Available
    );
    assert_eq!(
        composition_commit.input_generation,
        next_prompt.input_generation.wrapping_add(1)
    );
    let mut committed_bytes = [0_u8; 11];
    tokio::time::timeout(
        Duration::from_secs(2),
        output.read_exact(&mut committed_bytes),
    )
    .await
    .expect("committed text has a bounded writer receipt")
    .expect("committed text reaches the same acknowledged writer");
    assert_eq!(&committed_bytes, b"composition");

    let unknown_line_commit = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: UiSessionBinding {
                session_id: session_id.clone(),
                owner: next_prompt.owner.clone(),
                session_generation: next_prompt.session_generation,
            },
            expected_context: None,
            projection: UiInputProjection::Invalidates,
            event: TerminalInputEvent::Text("unknown".to_owned()),
        })
        .await
        .expect("untrusted committed Text still reaches the same writer");
    assert_eq!(
        unknown_line_commit.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::UiInputProjectionInvalidated
        )
    );
    let mut unknown_bytes = [0_u8; 7];
    tokio::time::timeout(
        Duration::from_secs(2),
        output.read_exact(&mut unknown_bytes),
    )
    .await
    .expect("unknown-line text has a bounded writer receipt")
    .expect("unknown-line text still reaches PTY without restoring projection");
    assert_eq!(&unknown_bytes, b"unknown");

    let after_untracked_edit = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: UiSessionBinding {
                session_id: session_id.clone(),
                owner: next_prompt.owner.clone(),
                session_generation: next_prompt.session_generation,
            },
            expected_context: None,
            projection: UiInputProjection::Invalidates,
            event: TerminalInputEvent::Paste("paste".to_owned()),
        })
        .await
        .expect("untracked UI paste still reaches the terminal");
    assert_eq!(
        after_untracked_edit.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::UiInputProjectionInvalidated
        )
    );

    manager
        .close_all()
        .await
        .expect("close test session resources");
}

/// Agent pane 可按精确 owner 与 generation 写入，但不会得到 UI shell suggestion 能力。
///
/// @author kongweiguang
#[tokio::test]
async fn agent_view_input_requires_exact_owner_and_generation() {
    let (manager, factory, session_id) = create_agent_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send Agent shell prompt markers");
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    assert!(matches!(prompt.owner, SessionOwner::Agent { .. }));
    assert_eq!(
        prompt.availability,
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::NonUiOwner)
    );

    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: prompt.owner.clone(),
        session_generation: prompt.session_generation,
    };
    let accepted = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: binding.clone(),
            expected_context: None,
            projection: UiInputProjection::Invalidates,
            event: TerminalInputEvent::Text("agent-input".to_owned()),
        })
        .await
        .expect("matching visible Agent binding may write");
    assert_eq!(accepted.owner, prompt.owner);
    assert_eq!(
        accepted.input_generation,
        prompt.input_generation.wrapping_add(1)
    );
    assert_eq!(
        accepted.availability,
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::NonUiOwner)
    );
    let mut received = [0_u8; 11];
    tokio::time::timeout(Duration::from_secs(1), output.read_exact(&mut received))
        .await
        .expect("Agent input reaches the shared writer")
        .expect("read Agent input");
    assert_eq!(&received, b"agent-input");

    let stale = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: UiSessionBinding {
                session_id: session_id.clone(),
                owner: binding.owner.clone(),
                session_generation: binding.session_generation.wrapping_add(1),
            },
            expected_context: None,
            projection: UiInputProjection::Invalidates,
            event: TerminalInputEvent::Text("stale-generation".to_owned()),
        })
        .await
        .expect_err("old Agent pane generation must not reach a reconnected runtime");
    assert_eq!(
        stale.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let mut unexpected = [0_u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), output.read(&mut unexpected))
            .await
            .is_err(),
        "stale generation must not write bytes"
    );

    let wrong_owner = manager
        .write_ui_input(UiTerminalInputRequest {
            binding: UiSessionBinding {
                session_id: session_id.clone(),
                owner: SessionOwner::Mcp,
                session_generation: binding.session_generation,
            },
            expected_context: None,
            projection: UiInputProjection::Invalidates,
            event: TerminalInputEvent::Text("wrong-owner".to_owned()),
        })
        .await
        .expect_err("background MCP owner cannot use the visible-view write port");
    assert_eq!(
        wrong_owner.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), output.read(&mut unexpected))
            .await
            .is_err(),
        "wrong owner must not write bytes"
    );

    manager.close_all().await.expect("close Agent test session");
}

/// 受管 UI 命令必须在写入前原子复验 generation，防止检查后重连把旧脚本送入新 transport。
///
/// @author kongweiguang
#[tokio::test]
async fn managed_command_rejects_stale_ui_binding_before_injection() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send prompt markers");
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let stale_binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: prompt.owner.clone(),
        session_generation: prompt.session_generation.wrapping_add(1),
    };

    let result = manager
        .execute_shell_command(
            ShellCommandRequest {
                session_id: session_id.clone(),
                expected_ui_binding: Some(stale_binding),
                command_id: "stale-ui-binding".to_owned(),
                command: "echo must-not-run".to_owned(),
                timeout: Duration::from_secs(1),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        result
            .expect_err("stale UI generation must be rejected")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    let after = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("context after rejected request");
    assert!(after.prompt_ready);
    assert!(after.prompt_line_empty);
    let mut first_byte = [0_u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), output.read(&mut first_byte))
            .await
            .is_err(),
        "a rejected request must not write command bytes"
    );

    let valid_binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: prompt.owner,
        session_generation: prompt.session_generation,
    };
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let service = Arc::clone(&manager);
    let command = tokio::spawn(async move {
        service
            .execute_shell_command(
                ShellCommandRequest {
                    session_id: session_id.clone(),
                    expected_ui_binding: Some(valid_binding),
                    command_id: "valid-ui-binding".to_owned(),
                    command: "echo accepted".to_owned(),
                    timeout: Duration::from_secs(5),
                },
                task_cancellation,
            )
            .await
    });
    let mut injected = [0_u8; 256];
    let injected_bytes = tokio::time::timeout(Duration::from_secs(2), output.read(&mut injected))
        .await
        .expect("matching UI binding reaches the shared writer")
        .expect("read accepted command bytes");
    assert!(injected_bytes > 0);
    cancellation.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(2), command)
        .await
        .expect("cancelled command future returns")
        .expect("command task joins")
        .expect("accepted command returns an outcome");
    assert_eq!(
        outcome,
        ShellCommandOutcome::Unknown {
            command_id: "valid-ui-binding".to_owned(),
            reason: ShellCommandUnknownReason::Cancelled,
        }
    );

    manager
        .close_all()
        .await
        .expect("close test session resources");
}

/// Alternate-screen 或未知 shell dialect 都不能被误当成可提供命令建议的 shell prompt。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_context_rejects_alternate_screen_and_unknown_dialect() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Unknown).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send prompt markers");
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    assert_eq!(
        prompt.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::UnsupportedShellDialect
        )
    );

    output
        .write_all(b"\x1b[?1049h")
        .await
        .expect("enter alternate screen");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let context = manager
                .terminal_command_context(session_id.clone())
                .await
                .expect("query alternate-screen context");
            if context.alternate_screen {
                assert_eq!(
                    context.availability,
                    TerminalCommandAvailability::Unavailable(
                        TerminalCommandUnavailableReason::AlternateScreen
                    )
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("alternate screen mode observed");

    manager
        .close_all()
        .await
        .expect("close test session resources");
}

/// OSC prompt 变化会取消仍在背压队列中的旧 suggestion，且 reader 不需等 writer 空间。
///
/// @author kongweiguang
#[tokio::test]
async fn prompt_change_cancels_pending_ui_write_without_blocking_reader() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut endpoint = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    endpoint
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send initial prompt markers");
    let initial_prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    assert_eq!(
        initial_prompt.availability,
        TerminalCommandAvailability::Available
    );

    // 填满反向 duplex 缓冲，使接下来的 UI 写意图确实停在有界 writer 队列。
    let buffered_output = vec![b'x'; 4096];
    manager
        .write(session_id.clone(), buffered_output.clone())
        .await
        .expect("fill bounded writer buffer");
    endpoint
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send new prompt markers");
    let prompt =
        wait_for_prompt_after(manager.as_ref(), &session_id, initial_prompt.prompt_epoch).await;

    let session = session_id.clone();
    let service = Arc::clone(&manager);
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: prompt.owner.clone(),
        session_generation: prompt.session_generation,
    };
    let expected_context = prompt.version();
    let expected_input_generation = prompt.input_generation.wrapping_add(1);
    let pending_write = tokio::spawn(async move {
        service
            .write_ui_input(UiTerminalInputRequest {
                binding,
                expected_context: Some(expected_context),
                projection: UiInputProjection::Tracked,
                event: TerminalInputEvent::Paste("stale-suggestion".repeat(512)),
            })
            .await
    });
    let registered =
        wait_for_input_generation(manager.as_ref(), &session, expected_input_generation).await;
    assert_eq!(
        registered.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::InputWritePending
        )
    );

    endpoint
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("advance shell prompt while UI writer is backpressured");
    let result = tokio::time::timeout(Duration::from_secs(2), pending_write)
        .await
        .expect("cancelled UI writer returns")
        .expect("UI input task joins");
    assert_eq!(
        result
            .expect_err("stale input must not continue into the new prompt")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let changed = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read context after cancelled write");
    assert!(changed.prompt_ready);
    assert_eq!(
        changed.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::ExternalInputObserved
        )
    );

    let mut received = vec![0; buffered_output.len()];
    endpoint
        .read_exact(&mut received)
        .await
        .expect("drain bounded writer buffer");
    assert_eq!(received, buffered_output);
    manager
        .close_all()
        .await
        .expect("close test session resources");
}

/// raw write receipt 保留准入前的真实空 prompt，而不读取或重建终端屏幕文本。
///
/// receipt 捕获与 input generation 递增必须在同一短临界区；后续 prompt 到达不应改写已接受
/// 输入的来源事实，因此命令历史可以安全地区分可确认与未知写入结果。
///
/// @author kongweiguang
#[tokio::test]
async fn raw_write_receipt_keeps_pre_input_prompt_state() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send shell prompt markers");
    let before = wait_for_prompt(manager.as_ref(), &session_id).await;
    assert!(before.prompt_ready);
    assert!(before.prompt_line_empty);

    let input = b"printf 'ok'\r".to_vec();
    let expected_bytes = input.len();
    let receipt = manager
        .write_with_receipt(session_id.clone(), input)
        .await
        .expect("write is fully acknowledged");
    let admission = receipt
        .admission
        .expect("production manager provides admission evidence");
    assert_eq!(receipt.acknowledged_bytes, expected_bytes);
    assert_eq!(admission.session_id, session_id);
    assert_eq!(admission.prompt_epoch, before.prompt_epoch);
    assert!(admission.shell_integration_enabled);
    assert!(admission.shell_integration_observed);
    assert!(admission.prompt_ready);
    assert!(admission.prompt_line_empty);
    assert_eq!(
        admission.accepted_input_generation,
        admission.input_generation_before.wrapping_add(1)
    );

    let mut received = vec![0; expected_bytes];
    output
        .read_exact(&mut received)
        .await
        .expect("transport peer receives exact raw bytes");
    assert_eq!(received, b"printf 'ok'\r");

    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("advance the shell to the next prompt");
    let next = wait_for_prompt_after(manager.as_ref(), &session_id, admission.prompt_epoch).await;
    assert!(next.prompt_epoch > admission.prompt_epoch);
    assert!(admission.prompt_line_empty);

    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close test session resources");
}

/// 原始输入跨过有界 duplex 后被 prompt 取消时，必须报告可能已送达而不是安全失败。
///
/// @author kongweiguang
#[tokio::test]
async fn partially_written_raw_input_reports_unknown_outcome() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("send shell prompt markers");
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let service = Arc::clone(&manager);
    let write_session_id = session_id.clone();
    let input = vec![b'x'; 8192];
    let write_task =
        tokio::spawn(async move { service.write_with_receipt(write_session_id, input).await });
    let pending = wait_for_input_generation(
        manager.as_ref(),
        &session_id,
        prompt.input_generation.wrapping_add(1),
    )
    .await;
    assert_eq!(
        pending.availability,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::InputWritePending
        )
    );

    output
        .write_all(b"\x1b]133;A\x07\x1b]133;B\x07")
        .await
        .expect("advance shell while raw input is backpressured");
    let result = tokio::time::timeout(Duration::from_secs(2), write_task)
        .await
        .expect("cancelled writer returns")
        .expect("writer task joins");
    assert_eq!(
        result
            .expect_err("partial input cannot be represented as a successful receipt")
            .public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );

    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close test session resources");
}

/// 验证有效可见绑定能滚动，而错误 owner 或旧 generation 不改变网格 revision/offset。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_scroll_requires_exact_visible_binding_and_generation() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let output_text = (0..48)
        .map(|line| format!("SCROLL-SEED-{line:02}\r\n"))
        .collect::<String>();
    output
        .write_all(output_text.as_bytes())
        .await
        .expect("send scrollback fixture");
    let bottom = wait_for_render_marker(manager.as_ref(), &session_id, "SCROLL-SEED-47").await;
    assert_eq!(bottom.scrollback_offset, 0);

    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };
    manager
        .scroll_ui(binding.clone(), ScrollRequest { lines: 1 })
        .await
        .expect("scroll current UI view");
    let scrolled = manager
        .render_snapshot(
            session_id.clone(),
            RenderSnapshotRequest {
                since_revision: Some(bottom.revision),
            },
        )
        .await
        .expect("read scrolled grid");
    assert_eq!(scrolled.scrollback_offset, 1);
    assert!(scrolled.revision > bottom.revision);

    let mut wrong_owner = binding.clone();
    wrong_owner.owner = SessionOwner::Mcp;
    assert_eq!(
        manager
            .scroll_ui(wrong_owner, ScrollRequest { lines: 1 })
            .await
            .expect_err("MCP owner cannot use visible-view scroll")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    let mut stale_generation = binding;
    stale_generation.session_generation = stale_generation.session_generation.wrapping_add(1);
    assert_eq!(
        manager
            .scroll_ui(stale_generation, ScrollRequest { lines: 1 })
            .await
            .expect_err("stale runtime generation cannot scroll")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    let after_rejections = manager
        .render_snapshot(
            session_id.clone(),
            RenderSnapshotRequest {
                since_revision: Some(scrolled.revision),
            },
        )
        .await
        .expect("rejected scrolls leave grid untouched");
    assert_eq!(after_rejections.revision, scrolled.revision);
    assert_eq!(
        after_rejections.scrollback_offset,
        scrolled.scrollback_offset
    );

    let visible_rows = |snapshot: &RenderSnapshot| {
        snapshot
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    let anchored_rows = visible_rows(&after_rejections);
    output
        .write_all(b"SCROLL-CONTINUE-48\r\n")
        .await
        .expect("continue output while the view is scrolled");
    let continued = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read viewport during continued output");
            let history = manager
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 128,
                        max_bytes: Some(8192),
                    },
                )
                .await
                .expect("read bounded history while viewport is anchored");
            if snapshot.revision > after_rejections.revision
                && history.text.contains("SCROLL-CONTINUE-48")
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("continued output reaches scrollback while view stays anchored");
    assert!(continued.scrollback_offset > after_rejections.scrollback_offset);
    assert_eq!(visible_rows(&continued), anchored_rows);

    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close test session resources");
}

/// 验证绝对跳转以当前 history 大小为边界，且旧 generation 不能继续改写 viewport。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_scroll_to_offset_clamps_and_rejects_stale_binding() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let output_text = (0..48)
        .map(|line| format!("JUMP-SEED-{line:02}\r\n"))
        .collect::<String>();
    output
        .write_all(output_text.as_bytes())
        .await
        .expect("send scrollback fixture");
    wait_for_render_marker(manager.as_ref(), &session_id, "JUMP-SEED-47").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };

    manager
        .scroll_ui_to_offset(binding.clone(), 7)
        .await
        .expect("jump to an in-range history offset");
    let jumped = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read jumped viewport");
    assert_eq!(jumped.scrollback_offset, 7);
    assert!(jumped.history_size >= jumped.scrollback_offset);

    manager
        .scroll_ui_to_offset(binding.clone(), usize::MAX)
        .await
        .expect("out-of-range offset clamps to available history");
    let clamped = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read clamped viewport");
    assert_eq!(clamped.scrollback_offset, clamped.history_size);

    let mut stale_binding = binding;
    stale_binding.session_generation = stale_binding.session_generation.wrapping_add(1);
    assert_eq!(
        manager
            .scroll_ui_to_offset(stale_binding, 0)
            .await
            .expect_err("stale view cannot jump the viewport")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let unchanged = manager
        .render_snapshot(session_id, RenderSnapshotRequest::default())
        .await
        .expect("verify stale jump left viewport unchanged");
    assert_eq!(unchanged.revision, clamped.revision);
    assert_eq!(unchanged.scrollback_offset, clamped.scrollback_offset);

    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close test session resources");
}

/// 已取消的 SessionManager 搜索在等待 session 状态锁前返回取消，不启动历史扫描。
///
/// @author kongweiguang
#[tokio::test]
async fn terminal_search_rejects_pre_cancelled_request() {
    let (manager, _factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = manager
        .search_cancellable(
            session_id,
            TerminalSearchRequest {
                query: "anything".to_owned(),
                case_sensitive: false,
                regex: false,
                backwards: false,
                max_matches: 16,
            },
            cancellation,
        )
        .await
        .expect_err("pre-cancelled search must not return an empty successful result");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close test session resources");
}

/// 验证 Exited 保留历史可供当前 view 滚动，重连后旧 generation 失效。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_scroll_rejects_old_binding_after_reconnect_and_exited_session() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut first_output = factory
        .output
        .lock()
        .await
        .take()
        .expect("first test output stream");
    let historical_output = (0..48)
        .map(|line| format!("RECONNECT-SCROLL-SEED-{line:02}\r\n"))
        .collect::<String>();
    first_output
        .write_all(historical_output.as_bytes())
        .await
        .expect("send initial output");
    let first_bottom =
        wait_for_render_marker(manager.as_ref(), &session_id, "RECONNECT-SCROLL-SEED-47").await;
    let initial_context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read initial session binding");
    let old_binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: initial_context.owner.clone(),
        session_generation: initial_context.session_generation,
    };

    drop(first_output);
    wait_for_lifecycle(manager.as_ref(), &session_id, SessionLifecycle::Exited).await;
    manager
        .scroll_ui(old_binding.clone(), ScrollRequest { lines: 1 })
        .await
        .expect("Exited session history remains available to the current view");
    let exited_history = manager
        .render_snapshot(
            session_id.clone(),
            RenderSnapshotRequest {
                since_revision: Some(first_bottom.revision),
            },
        )
        .await
        .expect("read scrolled history after child exit");
    assert_eq!(exited_history.scrollback_offset, 1);
    assert!(exited_history.history_size >= exited_history.scrollback_offset);
    assert!(exited_history.revision > first_bottom.revision);

    let reconnected = manager
        .reconnect(session_id.clone())
        .await
        .expect("reconnect the existing session target");
    assert_eq!(reconnected.session_id, session_id);
    let second_output = factory
        .output
        .lock()
        .await
        .take()
        .expect("reconnected test output stream");
    let current_context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read new runtime binding");
    assert!(current_context.session_generation > old_binding.session_generation);
    assert_eq!(current_context.owner, old_binding.owner);
    let before_stale_scroll = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read grid after reconnect");
    assert_eq!(
        manager
            .scroll_ui(old_binding, ScrollRequest { lines: 1 })
            .await
            .expect_err("pre-reconnect view cannot scroll the new runtime")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let after_stale_scroll = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("verify stale scroll had no grid effect");
    assert_eq!(after_stale_scroll.revision, before_stale_scroll.revision);
    assert_eq!(after_stale_scroll.scrollback_offset, 0);

    let current_binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: current_context.owner,
        session_generation: current_context.session_generation,
    };
    drop(second_output);
    wait_for_lifecycle(manager.as_ref(), &session_id, SessionLifecycle::Exited).await;
    let before_exited_scroll = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read grid for exited session");
    manager
        .scroll_ui(current_binding, ScrollRequest { lines: 1 })
        .await
        .expect("current Exited binding can scroll its retained view");
    let after_exited_scroll = manager
        .render_snapshot(session_id, RenderSnapshotRequest::default())
        .await
        .expect("verify exited scroll only changes view state");
    assert!(after_exited_scroll.revision > before_exited_scroll.revision);
    assert_eq!(after_exited_scroll.scrollback_offset, 0);

    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close reconnected test session resources");
}

/// Failed reader 仍保留已收到的历史行，视口滚动只读引擎，不依赖子进程继续运行。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_scroll_keeps_failed_session_history_available() {
    let output_text = (0..48)
        .map(|line| format!("FAILED-SCROLL-SEED-{line:02}\r\n"))
        .collect::<String>();
    let factory = Arc::new(FailedOutputFactory {
        bytes: output_text.into_bytes(),
    });
    let manager = Arc::new(SessionManager::new(
        factory,
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid terminal size"),
            initial_palette: None,
        })
        .await
        .expect("create failing-reader session");
    let session_id = info.session_id;
    let failed_info =
        wait_for_lifecycle(manager.as_ref(), &session_id, SessionLifecycle::Failed).await;
    let before =
        wait_for_render_marker(manager.as_ref(), &session_id, "FAILED-SCROLL-SEED-47").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read failed session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: failed_info.owner,
        session_generation: context.session_generation,
    };

    manager
        .scroll_ui(binding, ScrollRequest { lines: 1 })
        .await
        .expect("Failed session history remains scrollable");
    let after = manager
        .render_snapshot(
            session_id.clone(),
            RenderSnapshotRequest {
                since_revision: Some(before.revision),
            },
        )
        .await
        .expect("read failed session history after scroll");
    assert_eq!(after.scrollback_offset, 1);
    assert!(after.revision > before.revision);

    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close failed session resources");
}

/// 已重连但随后 Exited 的 session 可接受视口滚动，且滚动不会写入新 transport。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_scroll_on_reconnected_exited_session_does_not_write_transport() {
    let factory = Arc::new(TriggeredEofFactory {
        output_peer: Mutex::new(None),
        eof_triggers: Mutex::new(Vec::new()),
    });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid terminal size"),
            initial_palette: None,
        })
        .await
        .expect("create session with gated EOF");
    let session_id = info.session_id;
    factory.finish_current_reader().await;
    wait_for_lifecycle(manager.as_ref(), &session_id, SessionLifecycle::Exited).await;
    let first_generation = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read first generation");
    let reconnected = manager
        .reconnect(session_id.clone())
        .await
        .expect("reconnect same target");
    assert!(reconnected.lifecycle == SessionLifecycle::Running);
    factory.finish_current_reader().await;
    wait_for_lifecycle(manager.as_ref(), &session_id, SessionLifecycle::Exited).await;

    let current = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read reconnected exited generation");
    assert!(current.session_generation > first_generation.session_generation);
    let before = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read empty reconnected terminal view");
    manager
        .scroll_ui(
            UiSessionBinding {
                session_id: session_id.clone(),
                owner: current.owner,
                session_generation: current.session_generation,
            },
            ScrollRequest { lines: 1 },
        )
        .await
        .expect("current Exited view remains scrollable");
    let after = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read view after Exited scroll");
    assert!(after.revision > before.revision);
    assert_eq!(after.scrollback_offset, 0);

    let mut peer = factory
        .output_peer
        .lock()
        .await
        .take()
        .expect("observe writer peer from reconnected runtime");
    let mut unexpected = [0_u8; 1];
    match tokio::time::timeout(Duration::from_millis(20), peer.read(&mut unexpected)).await {
        Err(_) | Ok(Ok(0)) => {}
        Ok(Ok(read)) => {
            panic!("scrolling an Exited view emitted {read} byte(s) to the new transport")
        }
        Ok(Err(error)) => panic!(
            "observing the exited transport failed with {:?}",
            error.kind()
        ),
    }

    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close reconnected exited session");
}

/// Closing 状态下拒绝新滚动请求，避免正在释放的 session 被视图继续操作。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_scroll_rejects_session_while_close_is_in_progress() {
    let factory = Arc::new(GatedCloseFactory {
        output: Mutex::new(None),
        close_entered: Arc::new(Notify::new()),
        close_release: Arc::new(Semaphore::new(0)),
    });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(80, 24, 0, 0).expect("valid terminal size"),
            initial_palette: None,
        })
        .await
        .expect("create close-gated session");
    let session_id = info.session_id;
    let output = factory
        .output
        .lock()
        .await
        .take()
        .expect("keep reader peer alive until close cancellation");
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read close-gated session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };
    let close_manager = Arc::clone(&manager);
    let close_session_id = session_id.clone();
    let close_task = tokio::spawn(async move {
        close_manager
            .close_until(close_session_id, Instant::now() + Duration::from_secs(3))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), factory.close_entered.notified())
        .await
        .expect("close reached transport gate");
    let closing =
        wait_for_lifecycle(manager.as_ref(), &session_id, SessionLifecycle::Closing).await;
    let before = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read view while close is gated");
    assert_eq!(
        manager
            .scroll_ui(binding, ScrollRequest { lines: 1 })
            .await
            .expect_err("Closing session must reject a new view operation")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let after = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("verify rejected scroll leaves grid untouched");
    assert_eq!(closing.lifecycle, SessionLifecycle::Closing);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.scrollback_offset, before.scrollback_offset);

    drop(output);
    factory.close_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), close_task)
        .await
        .expect("close gate released")
        .expect("close task joins")
        .expect("session closes cleanly");
}

/// 输出推进、历史裁剪和网格重排后重新导航必须使用新结果坐标。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_search_recomputes_after_output_trim_and_resize() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };

    let old_history = (0..50)
        .map(|line| format!("SEARCH-OLD-{line:02}\r\n"))
        .collect::<String>();
    output
        .write_all(old_history.as_bytes())
        .await
        .expect("write old searchable history");
    let bottom = wait_for_render_marker(manager.as_ref(), &session_id, "SEARCH-OLD-49").await;
    let first = manager
        .search_ui_cancellable(
            binding.clone(),
            ui_search_request("SEARCH-", None),
            CancellationToken::new(),
        )
        .await
        .expect("search current UI session and jump to its oldest match");
    assert_eq!(first.result.matches.len(), 50);
    assert_eq!(first.result.selected_match, Some(0));
    assert!(first.revision > bottom.revision);
    let first_view = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read viewport positioned at the initial match");
    assert_eq!(first_view.revision, first.revision);
    let first_line = first.result.matches[0].range.start.line;
    assert!(
        rendered_row_text(&first_view, first_line)
            .expect("initial match is visible")
            .contains("SEARCH-OLD-00")
    );

    let mut changed_history = (0..30)
        .map(|line| format!("FILL-{line:02}\r\n"))
        .collect::<String>();
    changed_history.push_str("SEARCH-NEW-A\r\nSEARCH-NEW-B\r\nSEARCH-NEW-C\r\n");
    output
        .write_all(changed_history.as_bytes())
        .await
        .expect("advance terminal output beyond the old matches");
    wait_for_history_marker(manager.as_ref(), &session_id, "SEARCH-NEW-C").await;
    manager
        .set_scrollback_limit(session_id.clone(), 2)
        .await
        .expect("trim obsolete search history");
    manager
        .resize_ui_view(
            binding.clone(),
            TerminalSize::new(80, 8, 0, 0).expect("valid smaller grid"),
        )
        .await
        .expect("reflow current visible session");

    let navigated = manager
        .search_ui_cancellable(
            binding.clone(),
            ui_search_request("SEARCH-", Some(1)),
            CancellationToken::new(),
        )
        .await
        .expect("rerun navigation against the trimmed and resized grid");
    assert_eq!(navigated.result.matches.len(), 3);
    assert_eq!(navigated.result.selected_match, Some(1));
    let navigated_view = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read fresh navigation viewport");
    assert_eq!(navigated_view.revision, navigated.revision);
    let selected_line = navigated.result.matches[1].range.start.line;
    assert!(
        rendered_row_text(&navigated_view, selected_line)
            .expect("fresh selected match is visible")
            .contains("SEARCH-NEW-B")
    );
    let navigated_history = manager
        .plaintext_snapshot(
            session_id.clone(),
            PlainTextSnapshotRequest {
                include_scrollback: true,
                max_lines: 256,
                max_bytes: Some(64 * 1024),
            },
        )
        .await
        .expect("read retained history for all fresh search coordinates");
    assert!(!navigated_history.truncated);
    assert!(navigated.result.matches.iter().all(|matched| {
        historical_row_text(&navigated_history, matched.range.start.line)
            .is_some_and(|text| text.contains("SEARCH-NEW-"))
    }));

    let clamped = manager
        .search_ui_cancellable(
            binding,
            ui_search_request("SEARCH-", Some(usize::MAX)),
            CancellationToken::new(),
        )
        .await
        .expect("clamp stale target index to the current result list");
    assert_eq!(clamped.result.selected_match, Some(2));
    let clamped_view = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read viewport for clamped target");
    assert_eq!(clamped_view.revision, clamped.revision);
    assert!(
        rendered_row_text(&clamped_view, clamped.result.matches[2].range.start.line)
            .expect("clamped latest match is visible")
            .contains("SEARCH-NEW-C")
    );

    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close search test session");
}

/// 旧 UI binding 和取消请求必须在搜索定位前拒绝，不能改变可见 viewport。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_search_rejects_stale_binding_and_cancellation() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let searchable_history = (0..48)
        .map(|line| format!("GUARDED-SEARCH-{line:02}\r\n"))
        .collect::<String>();
    output
        .write_all(searchable_history.as_bytes())
        .await
        .expect("write history that would move the viewport");
    wait_for_render_marker(manager.as_ref(), &session_id, "GUARDED-SEARCH-47").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };
    let before = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read viewport before rejected searches");

    let mut old_binding = binding.clone();
    old_binding.session_generation = old_binding.session_generation.wrapping_add(1);
    assert_eq!(
        manager
            .search_ui_cancellable(
                old_binding,
                ui_search_request("GUARDED-SEARCH-00", None),
                CancellationToken::new(),
            )
            .await
            .expect_err("old view generation cannot search or scroll")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        manager
            .search_ui_cancellable(
                binding.clone(),
                ui_search_request("GUARDED-SEARCH-00", None),
                cancellation,
            )
            .await
            .expect_err("cancelled UI search cannot move the viewport")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );

    let cancelled_gate = UiTerminalSearchCommitGate::new();
    assert!(cancelled_gate.cancel());
    let mut request = ui_search_request("GUARDED-SEARCH-00", Some(0));
    request.commit_gate = cancelled_gate;
    assert_eq!(
        manager
            .search_ui_cancellable(binding.clone(), request, CancellationToken::new(),)
            .await
            .expect_err("a cancelled commit gate cannot reveal even with a live task token")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );

    let after = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read viewport after rejected searches");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.scrollback_offset, before.scrollback_offset);
    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close rejected-search test session");
}

/// 输入预览在持续输出时以同一 engine 锁完成搜索，保留底部位置且不默认选中或定位命中。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_search_preview_preserves_viewport_during_continuous_output() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let history = (0..2500)
        .map(|line| {
            if line == 0 {
                "PREVIEW-ANCHOR\r\n".to_owned()
            } else {
                format!("HISTORY-{line:04}\r\n")
            }
        })
        .collect::<String>();
    output
        .write_all(history.as_bytes())
        .await
        .expect("write searchable history");
    wait_for_render_marker(manager.as_ref(), &session_id, "HISTORY-2499").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let output_task = tokio::spawn(async move {
        let _ = started_tx.send(());
        for line in 0..160 {
            output
                .write_all(format!("LIVE-{line:03}\r\n").as_bytes())
                .await
                .expect("write concurrent output");
            tokio::task::yield_now().await;
        }
        output
    });
    started_rx.await.expect("output writer started");

    let preview = manager
        .search_ui_cancellable(
            binding.clone(),
            ui_search_request_with_viewport(
                "PREVIEW-ANCHOR",
                None,
                UiTerminalSearchViewport::PreserveViewport,
            ),
            CancellationToken::new(),
        )
        .await
        .expect("preview search remains atomic while output advances");
    let refreshed = manager
        .search_ui_cancellable(
            binding,
            ui_search_request_with_viewport(
                "PREVIEW-ANCHOR",
                Some(0),
                UiTerminalSearchViewport::PreserveViewport,
            ),
            CancellationToken::new(),
        )
        .await
        .expect("view revision refresh retains its selected match without navigating");
    drop(output_task.await.expect("continuous writer completes"));
    let latest = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read viewport after stream and preview");

    assert_eq!(preview.result.matches.len(), 1);
    assert_eq!(preview.result.selected_match, None);
    assert_eq!(preview.reveal_safe_margin_reached, None);
    assert_eq!(refreshed.result.selected_match, Some(0));
    assert_eq!(refreshed.reveal_safe_margin_reached, None);
    assert_eq!(latest.scrollback_offset, 0);
    assert!(latest.revision >= preview.revision);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close preview search session");
}

/// 显式 Reveal 在同一引擎坐标系内把命中放到搜索浮层的顶部安全行以下。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_search_reveal_respects_overlay_safe_top_rows() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let history = (0..56)
        .map(|line| {
            if line == 8 {
                "SAFE-MARGIN-ANCHOR\r\n".to_owned()
            } else {
                format!("SAFE-HISTORY-{line:02}\r\n")
            }
        })
        .collect::<String>();
    output
        .write_all(history.as_bytes())
        .await
        .expect("write history above terminal screen");
    wait_for_render_marker(manager.as_ref(), &session_id, "SAFE-HISTORY-55").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };

    let revealed = manager
        .search_ui_cancellable(
            binding,
            ui_search_request_with_viewport(
                "SAFE-MARGIN-ANCHOR",
                Some(0),
                UiTerminalSearchViewport::Reveal { safe_top_rows: 4 },
            ),
            CancellationToken::new(),
        )
        .await
        .expect("reveal search scrolls to its fresh match");
    let view = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read search reveal viewport");
    let match_line = revealed.result.matches[0].range.start.line;
    let visible_row = view
        .rows
        .iter()
        .position(|row| row.absolute_line == match_line)
        .expect("selected match is visible");

    assert_eq!(revealed.reveal_safe_margin_reached, Some(true));
    assert!(
        visible_row >= 4,
        "match row {visible_row} overlaps the overlay"
    );
    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close safe-margin search session");
}

/// 没有 scrollback 时 Reveal 不伪造滚动成功，并明确回报布局需要切换浮层位置。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_search_reveal_reports_unavailable_safe_margin_without_history() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    output
        .write_all(b"NO-HISTORY-ANCHOR\r\n")
        .await
        .expect("write only a visible row");
    wait_for_render_marker(manager.as_ref(), &session_id, "NO-HISTORY-ANCHOR").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };

    let revealed = manager
        .search_ui_cancellable(
            binding,
            ui_search_request_with_viewport(
                "NO-HISTORY-ANCHOR",
                Some(0),
                UiTerminalSearchViewport::Reveal { safe_top_rows: 4 },
            ),
            CancellationToken::new(),
        )
        .await
        .expect("search still returns a match when scrollback is empty");
    let view = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read no-history viewport");

    assert_eq!(revealed.reveal_safe_margin_reached, Some(false));
    assert_eq!(view.scrollback_offset, 0);
    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close no-history search session");
}

/// UI 方向只改变首次 Reveal 的选中索引；结果序始终 chronological 以稳定后续导航索引。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_search_direction_keeps_match_indices_stable() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    output
        .write_all(b"DIR-MATCH-00\r\nDIR-MATCH-01\r\nDIR-MATCH-02\r\n")
        .await
        .expect("write ordered matches");
    wait_for_render_marker(manager.as_ref(), &session_id, "DIR-MATCH-02").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read visible session binding");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };

    let forward = manager
        .search_ui_cancellable(
            binding.clone(),
            ui_search_request("DIR-MATCH-", None),
            CancellationToken::new(),
        )
        .await
        .expect("find from the first match");
    let mut backwards_request = ui_search_request("DIR-MATCH-", None);
    backwards_request.request.backwards = true;
    let backwards = manager
        .search_ui_cancellable(binding.clone(), backwards_request, CancellationToken::new())
        .await
        .expect("find from the last match while preserving result order");
    let mut switched_direction = ui_search_request("DIR-MATCH-", Some(1));
    switched_direction.request.backwards = true;
    let switched_direction = manager
        .search_ui_cancellable(binding, switched_direction, CancellationToken::new())
        .await
        .expect("direction changes cannot remap an explicit selection index");

    let lines = |result: &UiTerminalSearchResult| {
        result
            .result
            .matches
            .iter()
            .map(|matched| matched.range.start.line)
            .collect::<Vec<_>>()
    };
    assert_eq!(forward.result.matches.len(), 3);
    assert_eq!(forward.result.selected_match, Some(0));
    assert_eq!(backwards.result.selected_match, Some(2));
    assert_eq!(switched_direction.result.selected_match, Some(1));
    assert_eq!(lines(&forward), lines(&backwards));
    assert_eq!(lines(&backwards), lines(&switched_direction));

    drop(output);
    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close direction search session");
}

/// Exited UI 会话仍可搜索保留历史并将命中定位到 viewport。
///
/// @author kongweiguang
#[tokio::test]
async fn exited_ui_history_remains_searchable_and_positionable() {
    let (manager, factory, session_id) = create_ui_session(RemoteShellDialect::Posix).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("test output stream");
    let history = (0..40)
        .map(|line| format!("EXITED-SEARCH-{line:02}\r\n"))
        .collect::<String>();
    output
        .write_all(history.as_bytes())
        .await
        .expect("write retained output before exit");
    wait_for_render_marker(manager.as_ref(), &session_id, "EXITED-SEARCH-39").await;
    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read view binding before exit");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: context.owner,
        session_generation: context.session_generation,
    };
    drop(output);
    wait_for_lifecycle(manager.as_ref(), &session_id, SessionLifecycle::Exited).await;

    let result = manager
        .search_ui_cancellable(
            binding,
            ui_search_request("EXITED-SEARCH-03", None),
            CancellationToken::new(),
        )
        .await
        .expect("search retained history after shell exit");
    assert_eq!(result.result.matches.len(), 1);
    assert_eq!(result.result.selected_match, Some(0));
    let view = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read Exited viewport positioned at the match");
    assert_eq!(view.revision, result.revision);
    assert!(
        rendered_row_text(&view, result.result.matches[0].range.start.line)
            .expect("Exited match remains visible")
            .contains("EXITED-SEARCH-03")
    );

    manager
        .close_all_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("close exited search test session");
}
