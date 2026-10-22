//! 通过生产 SessionManager reader/writer 路径核对命令块证据、重连和有界保留。
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
    AgentTerminalSubmissionRequest, COMMAND_BLOCK_RETENTION_LIMIT,
    CommandBlockOutputIncompleteReason, CommandBlockOutputState, CommandBlockSnapshot,
    CommandBlockState, CommandBlockText, CommandBlockTextUnknownReason, CommandBlockUnknownReason,
    CreateSessionRequest, MAX_COMMAND_BLOCK_OUTPUT_BYTES, SessionChangeKind, SessionService,
    SessionTransport, SessionTransportControl, SessionTransportFactory, SessionTransportOpenError,
    ShellCommandOutcome, ShellCommandRequest, TerminalCommandAvailability, TerminalCommandContext,
    UiInputProjection, UiSessionBinding, UiTerminalCommandSubmissionRequest,
    UiTerminalInputRequest,
};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::identity::{AgentSessionId, PaneId, SessionId, TabId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionOwner, SessionTarget,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use kerminal_terminal::input::TerminalInputEvent;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct CommandBlockFactory {
    output: Mutex<Option<DuplexStream>>,
    flush_gate: Mutex<Option<oneshot::Receiver<bool>>>,
    release_flush: Mutex<Option<oneshot::Sender<bool>>>,
    peer_close_requested: Arc<Notify>,
}

impl CommandBlockFactory {
    /// 门控首次 flush 并共享 peer close 信号，以分别复现 ACK 竞态和真实 reader EOF 收尾。
    ///
    /// @author kongweiguang
    fn new(gate_first_flush: bool) -> Self {
        let (release_flush, flush_gate) = if gate_first_flush {
            let (release, gate) = oneshot::channel();
            (Some(release), Some(gate))
        } else {
            (None, None)
        };
        Self {
            output: Mutex::new(None),
            flush_gate: Mutex::new(flush_gate),
            release_flush: Mutex::new(release_flush),
            peer_close_requested: Arc::new(Notify::new()),
        }
    }

    /// 释放被测试门控的 flush；生产 writer ACK 仍由其完整 write+flush 路径决定。
    ///
    /// @author kongweiguang
    async fn release_first_flush(&self, success: bool) {
        if let Some(release) = self.release_flush.lock().await.take() {
            let _ = release.send(success);
        }
    }
}

#[async_trait]
impl SessionTransportFactory for CommandBlockFactory {
    /// 使用真实 SessionManager reader 与可控 duplex PTY，关闭信号可让 peer 写半部产生 EOF。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: kerminal_domain::sessions::TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let (output, server) = tokio::io::duplex(4096);
        *self.output.lock().await = Some(output);
        let (reader, writer) = tokio::io::split(server);
        let writer: Pin<Box<dyn AsyncWrite + Send>> = match self.flush_gate.lock().await.take() {
            Some(flush_gate) => Box::pin(FlushGateWriter {
                inner: Box::pin(writer),
                flush_gate,
                released: false,
            }),
            None => Box::pin(writer),
        };
        Ok(SessionTransport::new(
            reader,
            writer,
            CommandBlockControl {
                peer_close_requested: Arc::clone(&self.peer_close_requested),
            },
        )
        .with_shell_dialect(RemoteShellDialect::Posix)
        .with_shell_integration_enabled(true))
    }
}

struct FlushGateWriter {
    inner: Pin<Box<dyn AsyncWrite + Send>>,
    flush_gate: oneshot::Receiver<bool>,
    released: bool,
}

impl AsyncWrite for FlushGateWriter {
    /// 先把输入字节送入 duplex，保留只有 flush 完成后才产生写入 ACK 的真实次序。
    ///
    /// @author kongweiguang
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().inner.as_mut().poll_write(context, bytes)
    }

    /// 在底层 flush 后额外等待测试许可，以确定性覆盖 reader 先观察到 OSC C 的窗口。
    ///
    /// @author kongweiguang
    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.inner.as_mut().poll_flush(context))?;
        if this.released {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut this.flush_gate).poll(context) {
            Poll::Ready(Ok(true)) => {
                this.released = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(false)) | Poll::Ready(Err(_)) => {
                this.released = true;
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "synthetic flush failure",
                )))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// 关闭时仍走底层 writer；测试只门控命令提交 flush，不改变 transport cleanup 语义。
    ///
    /// @author kongweiguang
    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().inner.as_mut().poll_shutdown(context)
    }
}

struct CommandBlockControl {
    peer_close_requested: Arc<Notify>,
}

#[async_trait]
impl SessionTransportControl for CommandBlockControl {
    /// 测试控制端不调整外部资源，避免把命令块断言与平台 resize 混在一起。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: kerminal_domain::sessions::TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 通知 fake peer 关闭写半部，使 reader 按真实 transport EOF 路径完成回收。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.peer_close_requested.notify_one();
        Ok(())
    }

    /// session 退出状态不作为 OSC 133 命令退出码的替代证据。
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

/// 构造经过 production SessionManager attach 的可见 UI 会话。
///
/// @author kongweiguang
async fn create_session(
    gate_first_flush: bool,
) -> (Arc<SessionManager>, Arc<CommandBlockFactory>, SessionId) {
    create_session_with_owner(
        gate_first_flush,
        SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
    )
    .await
}

/// 使用生产 SessionManager 创建指定 owner 的隔离 PTY，以验证 Agent writer 最终 CAS。
///
/// @author kongweiguang
async fn create_session_with_owner(
    gate_first_flush: bool,
    owner: SessionOwner,
) -> (Arc<SessionManager>, Arc<CommandBlockFactory>, SessionId) {
    let factory = Arc::new(CommandBlockFactory::new(gate_first_flush));
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
            owner,
            size: kerminal_domain::sessions::TerminalSize::new(80, 24, 0, 0)
                .expect("valid terminal size"),
        })
        .await
        .expect("create test session");
    (manager, factory, info.session_id)
}

/// 等待真实 OSC A/B prompt，调用方不以创建 session 当作 shell ready 的证据。
///
/// @author kongweiguang
async fn wait_for_prompt(
    service: &dyn SessionService,
    session_id: &SessionId,
) -> TerminalCommandContext {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let context = service
                .terminal_command_context(session_id.clone())
                .await
                .expect("query terminal command context");
            if context.prompt_ready
                && context.availability == TerminalCommandAvailability::Available
            {
                return context;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("OSC prompt markers become visible")
}

/// 按唯一 SessionId 查询权威快照，循环等待只读条件而不依赖 watch 合并事件数。
///
/// @author kongweiguang
async fn wait_for_snapshot(
    service: &dyn SessionService,
    session_id: &SessionId,
    predicate: impl Fn(&CommandBlockSnapshot) -> bool,
) -> CommandBlockSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = service
                .command_block_snapshot(session_id.clone(), &CancellationToken::new())
                .await
                .expect("query command block snapshot");
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected command block state becomes visible")
}

/// 发送真实 OSC 133 prompt/command 标记，避免在测试中直接调用私有状态机。
///
/// @author kongweiguang
async fn send_markers(output: &mut DuplexStream, markers: &[u8]) {
    output.write_all(markers).await.expect("write OSC markers");
}

/// 从同一份 prompt context 生成一次 UI command submission，确保输入版本与正文投影一致。
///
/// @author kongweiguang
fn submission(
    session_id: &SessionId,
    context: &TerminalCommandContext,
    projection: UiInputProjection,
    expected_context: Option<kerminal_application::sessions::TerminalCommandContextVersion>,
    command: &str,
) -> UiTerminalCommandSubmissionRequest {
    UiTerminalCommandSubmissionRequest {
        input: UiTerminalInputRequest {
            binding: UiSessionBinding {
                session_id: session_id.clone(),
                owner: context.owner.clone(),
                session_generation: context.session_generation,
            },
            expected_context,
            projection,
            event: TerminalInputEvent::Text("\r".to_owned()),
        },
        command: command.to_owned(),
    }
}

/// 可信 UI 文本只在 writer flush ACK 后与真实 OSC C 关联，并由真实 D 结束命令块。
///
/// @author kongweiguang
#[tokio::test]
async fn trusted_submission_tracks_real_command_start_and_finish() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    manager
        .write_ui_command_submission(submission(
            &session_id,
            &prompt,
            UiInputProjection::Tracked,
            Some(prompt.version()),
            "  echo trusted  ",
        ))
        .await
        .expect("write and flush Enter");
    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("receive Enter");
    assert_eq!(entered, *b"\r");

    send_markers(&mut output, b"\x1b]133;C\x07").await;
    let running = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    assert_eq!(
        running.blocks[0].command,
        CommandBlockText::Known("echo trusted".to_owned())
    );
    assert_eq!(running.blocks[0].state, CommandBlockState::Running);

    send_markers(&mut output, b"\x1b]133;D;0\x07").await;
    let finished = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot
            .blocks
            .first()
            .is_some_and(|block| matches!(block.state, CommandBlockState::Finished { .. }))
    })
    .await;
    assert_eq!(
        finished.blocks[0].state,
        CommandBlockState::Finished { exit_code: Some(0) }
    );
    assert!(finished.revision > running.revision);

    manager.close_all().await.expect("close test session");
}

/// 仅在终端启用 focus reporting 时发送协议字节，且不使已 ACK 的命令正文投影失效。
///
/// @author kongweiguang
#[tokio::test]
async fn focus_report_writes_protocol_without_mutating_command_projection() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b[?1004h\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let focus_in = UiTerminalInputRequest {
        binding: UiSessionBinding {
            session_id: session_id.clone(),
            owner: prompt.owner.clone(),
            session_generation: prompt.session_generation,
        },
        expected_context: None,
        projection: UiInputProjection::Invalidates,
        event: TerminalInputEvent::Focus(true),
    };
    let focus_context = manager
        .write_ui_input(focus_in)
        .await
        .expect("focus-in is accepted for the current UI binding");
    let mut focus_bytes = [0; 3];
    output
        .read_exact(&mut focus_bytes)
        .await
        .expect("focus-in reaches the shared terminal writer");
    assert_eq!(focus_bytes, *b"\x1b[I");
    assert_eq!(focus_context.version(), prompt.version());
    assert_eq!(focus_context.input_generation, prompt.input_generation);

    let command = manager
        .write_ui_command_submission_with_receipt(submission(
            &session_id,
            &focus_context,
            UiInputProjection::Tracked,
            Some(focus_context.version()),
            "echo focus-safe",
        ))
        .await
        .expect("command submission is ACKed after focus-in");
    assert!(command.command_associated);
    let mut enter = [0; 1];
    output
        .read_exact(&mut enter)
        .await
        .expect("command Enter reaches the terminal");
    assert_eq!(enter, *b"\r");
    let before_focus_out = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read command context before focus-out");

    let focus_out = UiTerminalInputRequest {
        binding: UiSessionBinding {
            session_id: session_id.clone(),
            owner: before_focus_out.owner.clone(),
            session_generation: before_focus_out.session_generation,
        },
        expected_context: None,
        projection: UiInputProjection::Invalidates,
        event: TerminalInputEvent::Focus(false),
    };
    let after_focus_out = manager
        .write_ui_input(focus_out)
        .await
        .expect("focus-out is accepted without a line-edit side effect");
    output
        .read_exact(&mut focus_bytes)
        .await
        .expect("focus-out reaches the shared terminal writer");
    assert_eq!(focus_bytes, *b"\x1b[O");
    assert_eq!(after_focus_out.version(), before_focus_out.version());
    assert_eq!(
        after_focus_out.input_generation,
        before_focus_out.input_generation
    );
    assert_eq!(
        after_focus_out.prompt_line_empty,
        before_focus_out.prompt_line_empty
    );
    assert_eq!(after_focus_out.availability, before_focus_out.availability);

    send_markers(&mut output, b"\x1b]133;C\x07").await;
    let running = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.first().is_some_and(|block| {
            block.command == CommandBlockText::Known("echo focus-safe".to_owned())
                && block.state == CommandBlockState::Running
        })
    })
    .await;
    send_markers(&mut output, b"\x1b]133;D;0\x07").await;
    wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.first().is_some_and(|block| {
            block.command == CommandBlockText::Known("echo focus-safe".to_owned())
                && matches!(
                    &block.state,
                    CommandBlockState::Finished { exit_code: Some(0) }
                )
        })
    })
    .await;
    assert_eq!(
        running.blocks[0].command,
        CommandBlockText::Known("echo focus-safe".to_owned())
    );

    manager.close_all().await.expect("close test session");
}

/// 受管命令持有 input gate 时丢弃瞬态焦点报告，避免焦点变化中断原命令或改写候选代次。
/// 完整消费生产编码器包装后的单行命令，不能把裸命令长度或 UI 正文捕获规则套到受管执行路径。
///
/// @author kongweiguang
#[tokio::test]
async fn focus_report_does_not_interrupt_managed_command_while_input_gate_is_busy() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b[?1004h\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: prompt.owner.clone(),
        session_generation: prompt.session_generation,
    };
    let service = Arc::clone(&manager);
    let command_session_id = session_id.clone();
    let command = tokio::spawn(async move {
        service
            .execute_shell_command(
                ShellCommandRequest {
                    session_id: command_session_id,
                    expected_ui_binding: Some(binding),
                    command_id: "focus-gate-command".to_owned(),
                    command: "echo focus-gate".to_owned(),
                    timeout: Duration::from_secs(5),
                },
                CancellationToken::new(),
            )
            .await
    });

    let mut injected = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while injected.len() < 1024 {
            let byte = output
                .read_u8()
                .await
                .expect("read complete managed command bytes");
            injected.push(byte);
            if byte == b'\r' {
                return;
            }
        }
        panic!("bounded fixture command must end with Enter");
    })
    .await
    .expect("managed command reaches the shared writer");
    assert!(injected.starts_with(b"eval "));
    assert_eq!(injected.last(), Some(&b'\r'));
    send_markers(&mut output, b"\x1b]133;C\x07").await;
    let running = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot
            .blocks
            .first()
            .is_some_and(|block| block.state == CommandBlockState::Running)
    })
    .await;
    let before_focus = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read managed command context before focus");
    let focus = UiTerminalInputRequest {
        binding: UiSessionBinding {
            session_id: session_id.clone(),
            owner: before_focus.owner.clone(),
            session_generation: before_focus.session_generation,
        },
        expected_context: None,
        projection: UiInputProjection::Invalidates,
        event: TerminalInputEvent::Focus(true),
    };
    let after_focus = manager
        .write_ui_input(focus)
        .await
        .expect("busy-gate focus report is safely dropped");
    assert_eq!(after_focus.version(), before_focus.version());
    assert_eq!(after_focus.input_generation, before_focus.input_generation);
    assert_eq!(
        after_focus.prompt_line_empty,
        before_focus.prompt_line_empty
    );
    assert_eq!(after_focus.availability, before_focus.availability);
    let unchanged = manager
        .command_block_snapshot(session_id.clone(), &CancellationToken::new())
        .await
        .expect("read command block after dropped focus report");
    assert_eq!(unchanged.revision, running.revision);
    assert_eq!(unchanged.blocks, running.blocks);
    let mut unexpected = [0; 3];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), output.read(&mut unexpected))
            .await
            .is_err(),
        "busy-gate focus report must not bypass the shared input gate"
    );

    send_markers(&mut output, b"\x1b]133;D;0\x07").await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), command)
            .await
            .expect("managed command completes")
            .expect("command task joins")
            .expect("managed command returns an outcome"),
        ShellCommandOutcome::Finished {
            command_id: "focus-gate-command".to_owned(),
            exit_code: 0,
        }
    );
    manager.close_all().await.expect("close test session");
}

/// 过期 prompt epoch 仍可保留终端输入语义，但 writer gate 必须拒绝把旧投影认作命令正文。
///
/// @author kongweiguang
#[tokio::test]
async fn command_submission_receipt_rejects_stale_prompt_epoch() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let mut stale = prompt.clone();
    stale.prompt_epoch = stale.prompt_epoch.wrapping_add(1);

    let receipt = manager
        .write_ui_command_submission_with_receipt(submission(
            &session_id,
            &stale,
            UiInputProjection::Tracked,
            Some(stale.version()),
            "echo stale-projection",
        ))
        .await
        .expect("ordinary input is still sent when the command projection is stale");
    assert!(!receipt.command_associated);
    assert_eq!(
        receipt.context.input_generation,
        prompt.input_generation + 1
    );

    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("stale projection still writes its Enter event");
    assert_eq!(entered, *b"\r");
    send_markers(&mut output, b"\x1b]133;C\x07").await;
    let snapshot = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    assert_eq!(
        snapshot.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::NotCaptured)
    );
    manager.close_all().await.expect("close test session");
}

/// 输出正文只来自同一引擎的 OSC C..D 转录，查询裁剪 UTF-8 且 Debug 不暴露输出内容。
///
/// @author kongweiguang
#[tokio::test]
async fn command_block_output_uses_bounded_osc_capture() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(
        &mut output,
        b"\x1b]133;C\x07first\x1b[31msynthetic-secret\x1b[0m\n\x1b]133;D;7\x07",
    )
    .await;
    let finished = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.first().is_some_and(|block| {
            matches!(
                block.state,
                CommandBlockState::Finished { exit_code: Some(7) }
            )
        })
    })
    .await;
    let block_id = finished.blocks[0].id;
    let captured = manager
        .command_block_output(
            session_id.clone(),
            block_id,
            MAX_COMMAND_BLOCK_OUTPUT_BYTES,
            &CancellationToken::new(),
        )
        .await
        .expect("query command transcript");
    assert_eq!(
        captured.state,
        CommandBlockOutputState::Complete { exit_code: Some(7) }
    );
    let text = captured
        .text
        .as_ref()
        .expect("captured output body")
        .as_str();
    assert!(text.contains("firstsynthetic-secret\n"));
    assert!(!text.contains("\x1b[31m"));
    assert_eq!(captured.returned_bytes, text.len());
    assert!(!captured.truncated);
    assert!(!format!("{captured:?}").contains("synthetic-secret"));

    let prefix = manager
        .command_block_output(session_id.clone(), block_id, 5, &CancellationToken::new())
        .await
        .expect("query bounded transcript prefix");
    assert_eq!(prefix.text.as_ref().unwrap().as_str(), "first");
    assert_eq!(prefix.returned_bytes, 5);
    assert!(prefix.truncated);
    assert!(
        manager
            .command_block_output(
                session_id.clone(),
                block_id,
                MAX_COMMAND_BLOCK_OUTPUT_BYTES + 1,
                &CancellationToken::new(),
            )
            .await
            .is_err()
    );

    manager.close_all().await.expect("close test session");
}

/// 多行 Agent preview 只有在目标 PTY 声明 bracketed paste 后才以一个粘贴帧加单次 Enter 写入。
///
/// @author kongweiguang
#[tokio::test]
async fn agent_submission_checks_owner_generation_and_multiline_paste_mode() {
    let agent_session_id = AgentSessionId::new();
    let (manager, factory, session_id) = create_session_with_owner(
        false,
        SessionOwner::Agent {
            agent_session_id: agent_session_id.clone(),
        },
    )
    .await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    let context = manager
        .command_block_snapshot(session_id.clone(), &CancellationToken::new())
        .await
        .expect("read current generation");

    for request in [
        AgentTerminalSubmissionRequest {
            session_id: session_id.clone(),
            agent_session_id: AgentSessionId::new(),
            expected_session_generation: context.session_generation,
            text: "single line".to_owned(),
        },
        AgentTerminalSubmissionRequest {
            session_id: session_id.clone(),
            agent_session_id: agent_session_id.clone(),
            expected_session_generation: context.session_generation.wrapping_add(1),
            text: "single line".to_owned(),
        },
    ] {
        assert!(
            manager
                .write_agent_submission(request, CancellationToken::new())
                .await
                .is_err()
        );
    }

    let multiline = "first line\nsecond\tline";
    let unbracketed = manager
        .write_agent_submission(
            AgentTerminalSubmissionRequest {
                session_id: session_id.clone(),
                agent_session_id: agent_session_id.clone(),
                expected_session_generation: context.session_generation,
                text: multiline.to_owned(),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("unsafe multi-line paste mode is rejected");
    assert_eq!(unbracketed.public_code().as_str(), "unsupported");

    let mut subscription = manager
        .subscribe(session_id.clone())
        .await
        .expect("subscribe to terminal output");
    output
        .write_all(b"\x1b[?2004h")
        .await
        .expect("enable bracketed paste on the fake terminal");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if subscription
                .recv(&CancellationToken::new())
                .await
                .expect("receive mode update")
                .is_some_and(|change| change.kind == SessionChangeKind::Output)
            {
                break;
            }
        }
    })
    .await
    .expect("terminal parser applies bracketed paste mode");

    let request = AgentTerminalSubmissionRequest {
        session_id: session_id.clone(),
        agent_session_id,
        expected_session_generation: context.session_generation,
        text: multiline.to_owned(),
    };
    assert!(!format!("{request:?}").contains("first line"));
    let receipt = manager
        .write_agent_submission(request, CancellationToken::new())
        .await
        .expect("write one bounded paste plus Enter");
    let mut received = vec![0; receipt.acknowledged_bytes];
    output
        .read_exact(&mut received)
        .await
        .expect("read the single submitted buffer");
    assert_eq!(received, b"\x1b[200~first line\nsecond\tline\x1b[201~\r");
    assert_eq!(
        receipt.acknowledged_bytes,
        b"\x1b[200~first line\nsecond\tline\x1b[201~\r".len()
    );
    manager.close_all().await.expect("close test session");
}

/// writer 已收正文但取消发生在 flush 前时报告未知结果，禁止把 Agent 粘贴误报为 ACK。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_agent_submission_after_write_is_outcome_unknown() {
    let agent_session_id = AgentSessionId::new();
    let (manager, factory, session_id) = create_session_with_owner(
        true,
        SessionOwner::Agent {
            agent_session_id: agent_session_id.clone(),
        },
    )
    .await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    let context = manager
        .command_block_snapshot(session_id.clone(), &CancellationToken::new())
        .await
        .expect("read current generation");
    let cancellation = CancellationToken::new();
    let writer_cancellation = cancellation.clone();
    let writer_manager = Arc::clone(&manager);
    let writer_session_id = session_id.clone();
    let writer_agent_id = agent_session_id.clone();
    let writer = tokio::spawn(async move {
        writer_manager
            .write_agent_submission(
                AgentTerminalSubmissionRequest {
                    session_id: writer_session_id,
                    agent_session_id: writer_agent_id,
                    expected_session_generation: context.session_generation,
                    text: "confirmed prompt".to_owned(),
                },
                writer_cancellation,
            )
            .await
    });

    let mut written = [0; 17];
    output
        .read_exact(&mut written)
        .await
        .expect("paste bytes reach PTY before flush ACK");
    assert_eq!(&written, b"confirmed prompt\r");
    cancellation.cancel();
    factory.release_first_flush(true).await;
    let error = writer
        .await
        .expect("Agent write task joins")
        .expect_err("post-write cancellation is not successful ACK");
    assert_eq!(error.public_code().as_str(), "outcome_unknown");
    manager.close_all().await.expect("close test session");
}

/// 未跟踪输入与敏感候选仍把 Enter 交给 PTY，但快照正文明确未知且不泄露候选。
///
/// @author kongweiguang
#[tokio::test]
async fn unsafe_or_untracked_text_never_suppresses_enter_or_enters_snapshot() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let first_prompt = wait_for_prompt(manager.as_ref(), &session_id).await;

    manager
        .write_ui_command_submission(submission(
            &session_id,
            &first_prompt,
            UiInputProjection::Invalidates,
            None,
            "echo untracked",
        ))
        .await
        .expect("untracked Enter still reaches transport");
    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("receive untracked Enter");
    assert_eq!(entered, *b"\r");
    send_markers(
        &mut output,
        b"\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07",
    )
    .await;
    let first = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
            && matches!(
                snapshot.blocks[0].state,
                CommandBlockState::Finished { exit_code: Some(0) }
            )
    })
    .await;
    assert_eq!(
        first.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::NotCaptured)
    );
    let next_prompt = wait_for_prompt(manager.as_ref(), &session_id).await;

    manager
        .write_ui_command_submission(submission(
            &session_id,
            &next_prompt,
            UiInputProjection::Tracked,
            Some(next_prompt.version()),
            "curl -H 'Authorization: Bearer synthetic-secret' https://example.invalid",
        ))
        .await
        .expect("sensitive Enter still reaches transport");
    output
        .read_exact(&mut entered)
        .await
        .expect("receive sensitive Enter");
    assert_eq!(entered, *b"\r");
    send_markers(&mut output, b"\x1b]133;C\x07\x1b]133;D;1\x07").await;
    let second = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 2
            && matches!(snapshot.blocks[1].state, CommandBlockState::Finished { .. })
    })
    .await;
    assert_eq!(
        second.blocks[1].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe)
    );
    let debug = format!("{:?}", second.blocks[1].command);
    assert!(!debug.contains("synthetic-secret"));

    manager.close_all().await.expect("close test session");
}

/// 在 OSC C 早于 flush ACK 时先发布未知正文，ACK 到达后只更新原块而不创建重复记录。
///
/// @author kongweiguang
#[tokio::test]
async fn command_text_waits_for_full_writer_ack_even_when_osc_c_arrives_first() {
    let (manager, factory, session_id) = create_session(true).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let mut subscription = manager
        .subscribe(session_id.clone())
        .await
        .expect("subscribe before command submission");

    let service = Arc::clone(&manager);
    let request = submission(
        &session_id,
        &prompt,
        UiInputProjection::Tracked,
        Some(prompt.version()),
        "echo ack-order",
    );
    let submission_task =
        tokio::spawn(async move { service.write_ui_command_submission(request).await });
    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("writer sends Enter before flush");
    assert_eq!(entered, *b"\r");
    send_markers(&mut output, b"\x1b]133;C\x07").await;

    let before_ack = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    assert_eq!(
        before_ack.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::WriteNotAcknowledged)
    );
    assert_eq!(
        subscription
            .recv(&CancellationToken::new())
            .await
            .expect("receive OSC C invalidation")
            .expect("subscription remains open")
            .kind,
        SessionChangeKind::Output
    );
    factory.release_first_flush(true).await;
    submission_task
        .await
        .expect("submission task joins")
        .expect("writer ACK returns");

    let after_ack = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.first().is_some_and(|block| {
            block.command == CommandBlockText::Known("echo ack-order".to_owned())
        })
    })
    .await;
    assert_eq!(after_ack.blocks.len(), 1);
    assert_eq!(after_ack.blocks[0].state, CommandBlockState::Running);
    assert_eq!(
        subscription
            .recv(&CancellationToken::new())
            .await
            .expect("receive ACK text invalidation")
            .expect("subscription remains open")
            .kind,
        SessionChangeKind::Output
    );
    send_markers(&mut output, b"\x1b]133;D;0\x07").await;
    wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.first().is_some_and(|block| {
            matches!(
                block.state,
                CommandBlockState::Finished { exit_code: Some(0) }
            )
        })
    })
    .await;
    manager.close_all().await.expect("close test session");
}

/// 无可信命令候选的普通 UI 输入仍由 shell 边界取消，部分写入结果保持未知。
///
/// @author kongweiguang
#[tokio::test]
async fn ordinary_ui_input_is_still_cancelled_by_shell_marker() {
    let (manager, factory, session_id) = create_session(true).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let request = UiTerminalInputRequest {
        binding: UiSessionBinding {
            session_id: session_id.clone(),
            owner: prompt.owner.clone(),
            session_generation: prompt.session_generation,
        },
        expected_context: Some(prompt.version()),
        projection: UiInputProjection::Tracked,
        event: TerminalInputEvent::Text("\r".to_owned()),
    };
    let service = Arc::clone(&manager);
    let input_task = tokio::spawn(async move { service.write_ui_input(request).await });

    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("ordinary input reaches transport");
    assert_eq!(entered, *b"\r");
    send_markers(&mut output, b"\x1b]133;C\x07").await;

    let error = input_task
        .await
        .expect("ordinary input task joins")
        .expect_err("unassociated in-flight input is cancelled by shell transition");
    assert_eq!(error.public_code().as_str(), "outcome_unknown");
    let snapshot = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    assert_eq!(
        snapshot.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::NotCaptured)
    );

    manager.close_all().await.expect("close test session");
}

/// 快速命令的 C/D/A/B 可先于 flush ACK，迟到 ACK 只确认旧块正文而不覆盖新 prompt。
///
/// @author kongweiguang
#[tokio::test]
async fn fast_command_markers_before_flush_ack_keep_finished_block() {
    let (manager, factory, session_id) = create_session(true).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let service = Arc::clone(&manager);
    let request = submission(
        &session_id,
        &prompt,
        UiInputProjection::Tracked,
        Some(prompt.version()),
        "echo completion-race",
    );
    let input_task = tokio::spawn(async move {
        service
            .write_ui_command_submission_with_receipt(request)
            .await
    });

    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("command Enter reaches transport");
    assert_eq!(entered, *b"\r");
    send_markers(&mut output, b"\x1b]133;C\x07").await;
    send_markers(&mut output, b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07").await;

    let before_ack = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.first().is_some_and(|block| {
            matches!(
                block.state,
                CommandBlockState::Finished { exit_code: Some(0) }
            )
        })
    })
    .await;
    assert_eq!(
        before_ack.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::WriteNotAcknowledged)
    );
    assert_eq!(
        before_ack.blocks[0].state,
        CommandBlockState::Finished { exit_code: Some(0) }
    );
    let new_prompt_before_ack = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read next prompt while original flush is pending");
    assert!(new_prompt_before_ack.prompt_ready);
    assert_ne!(
        new_prompt_before_ack.prompt_epoch, prompt.prompt_epoch,
        "OSC A must establish a new prompt before the old write ACK"
    );
    assert_eq!(
        new_prompt_before_ack.availability,
        TerminalCommandAvailability::Unavailable(
            kerminal_application::sessions::TerminalCommandUnavailableReason::InputWritePending
        )
    );

    factory.release_first_flush(true).await;
    let receipt = input_task
        .await
        .expect("submission task joins")
        .expect("the exact Enter write is acknowledged");
    assert!(receipt.command_associated);
    let after_ack = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.first().is_some_and(|block| {
            block.command == CommandBlockText::Known("echo completion-race".to_owned())
        })
    })
    .await;
    assert_eq!(
        after_ack.blocks[0].state,
        CommandBlockState::Finished { exit_code: Some(0) }
    );
    let new_prompt_after_ack = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read next prompt after old write ACK");
    assert_eq!(
        new_prompt_after_ack.version(),
        new_prompt_before_ack.version()
    );
    assert!(new_prompt_after_ack.prompt_ready);
    assert_eq!(
        new_prompt_after_ack.availability,
        TerminalCommandAvailability::Available
    );

    manager.close_all().await.expect("close test session");
}

/// 快速命令已完成后，writer flush 失败仍保留未知正文且不污染新 prompt。
///
/// @author kongweiguang
#[tokio::test]
async fn writer_flush_failure_after_fast_command_keeps_body_unknown() {
    let (manager, factory, session_id) = create_session(true).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let service = Arc::clone(&manager);
    let request = submission(
        &session_id,
        &prompt,
        UiInputProjection::Tracked,
        Some(prompt.version()),
        "echo writer-failure",
    );
    let input_task =
        tokio::spawn(async move { service.write_ui_command_submission(request).await });

    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("command Enter reaches transport");
    assert_eq!(entered, *b"\r");
    send_markers(
        &mut output,
        b"\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07",
    )
    .await;
    let before_failure = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    assert_eq!(
        before_failure.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::WriteNotAcknowledged)
    );

    factory.release_first_flush(false).await;
    let error = input_task
        .await
        .expect("input task joins after writer failure")
        .expect_err("flush error must remain an unknown write result");
    assert_eq!(error.public_code().as_str(), "outcome_unknown");
    let after_failure = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    assert_eq!(
        after_failure.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::WriteNotAcknowledged)
    );
    assert_eq!(
        after_failure.blocks[0].state,
        CommandBlockState::Finished { exit_code: Some(0) }
    );
    let prompt_after_failure = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read next prompt after prior flush failure");
    assert!(prompt_after_failure.prompt_ready);
    assert_eq!(
        prompt_after_failure.availability,
        TerminalCommandAvailability::Available
    );

    manager.close_all().await.expect("close test session");
}

/// 会话关闭撤销未 ACK 的正文，并在 control 发出关闭信号后关闭 peer 写半部以释放 reader。
///
/// @author kongweiguang
#[tokio::test]
async fn closing_session_keeps_unacknowledged_command_unknown() {
    let (manager, factory, session_id) = create_session(true).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    let request = submission(
        &session_id,
        &prompt,
        UiInputProjection::Tracked,
        Some(prompt.version()),
        "echo close-race",
    );
    let service = Arc::clone(&manager);
    let input_task =
        tokio::spawn(async move { service.write_ui_command_submission(request).await });

    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("command Enter reaches transport");
    assert_eq!(entered, *b"\r");
    send_markers(&mut output, b"\x1b]133;C\x07").await;
    let before_close = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    assert_eq!(
        before_close.blocks[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::WriteNotAcknowledged)
    );

    let close_requested = Arc::clone(&factory.peer_close_requested);
    let mut peer_close_task = tokio::spawn(async move {
        tokio::select! {
            _ = close_requested.notified() => output.shutdown().await,
            _ = tokio::time::sleep(Duration::from_secs(6)) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fake transport close signal was not received",
            )),
        }
    });
    let close_result = manager.close(session_id.clone()).await;
    let peer_close_result =
        match tokio::time::timeout(Duration::from_secs(2), &mut peer_close_task).await {
            Ok(joined) => joined.expect("peer close helper joins"),
            Err(_) => {
                peer_close_task.abort();
                let aborted = peer_close_task.await;
                assert!(
                    aborted.is_err(),
                    "timed out peer close helper is joined after abort"
                );
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fake transport peer close did not join within its bound",
                ))
            }
        };
    peer_close_result.expect("close signal shuts down the peer write half");
    close_result.expect("close cancels and joins the pending input writer");
    let error = input_task
        .await
        .expect("input task joins after close")
        .expect_err("close must not turn an unacknowledged write into success");
    assert_eq!(error.public_code().as_str(), "outcome_unknown");
}

/// 断连把 Running 关闭为 Unknown，重连推进 generation 并保留旧快照而不伪造完成。
///
/// @author kongweiguang
#[tokio::test]
async fn disconnect_and_reconnect_close_running_block_as_unknown() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory
        .output
        .lock()
        .await
        .take()
        .expect("first transport peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    let prompt = wait_for_prompt(manager.as_ref(), &session_id).await;
    manager
        .write_ui_command_submission(submission(
            &session_id,
            &prompt,
            UiInputProjection::Tracked,
            Some(prompt.version()),
            "sleep 99",
        ))
        .await
        .expect("send command Enter");
    let mut entered = [0; 1];
    output
        .read_exact(&mut entered)
        .await
        .expect("receive Enter");
    send_markers(&mut output, b"\x1b]133;C\x07").await;
    let running = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1 && snapshot.blocks[0].state == CommandBlockState::Running
    })
    .await;
    let old_generation = running.session_generation;
    let block_id = running.blocks[0].id;
    send_markers(&mut output, b"output before disconnect\n").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let captured = manager
                .command_block_output(
                    session_id.clone(),
                    block_id,
                    MAX_COMMAND_BLOCK_OUTPUT_BYTES,
                    &CancellationToken::new(),
                )
                .await
                .expect("query running transcript");
            if captured
                .text
                .as_ref()
                .is_some_and(|text| text.as_str().contains("output before disconnect\n"))
            {
                assert_eq!(captured.state, CommandBlockOutputState::Capturing);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("transcript bytes reach the active command capture");

    drop(output);
    let disconnected = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot
            .blocks
            .first()
            .is_some_and(|block| matches!(block.state, CommandBlockState::Unknown(_)))
    })
    .await;
    assert!(matches!(
        disconnected.blocks[0].state,
        CommandBlockState::Unknown(
            CommandBlockUnknownReason::Disconnected | CommandBlockUnknownReason::WriterFailed
        )
    ));
    let disconnected_output = manager
        .command_block_output(
            session_id.clone(),
            block_id,
            MAX_COMMAND_BLOCK_OUTPUT_BYTES,
            &CancellationToken::new(),
        )
        .await
        .expect("query disconnected command transcript");
    assert_eq!(
        disconnected_output.state,
        CommandBlockOutputState::Incomplete(CommandBlockOutputIncompleteReason::Disconnected)
    );

    manager
        .reconnect_cancellable(session_id.clone(), CancellationToken::new())
        .await
        .expect("explicitly reconnect the same session");
    let reconnected = manager
        .command_block_snapshot(session_id.clone(), &CancellationToken::new())
        .await
        .expect("query snapshot after reconnect");
    assert!(reconnected.session_generation > old_generation);
    assert_eq!(reconnected.blocks, disconnected.blocks);
    assert!(matches!(
        reconnected.blocks[0].state,
        CommandBlockState::Unknown(
            CommandBlockUnknownReason::Disconnected
                | CommandBlockUnknownReason::WriterFailed
                | CommandBlockUnknownReason::Reconnected
        )
    ));
    let reconnected_output = manager
        .command_block_output(
            session_id.clone(),
            block_id,
            MAX_COMMAND_BLOCK_OUTPUT_BYTES,
            &CancellationToken::new(),
        )
        .await
        .expect("query transcript retained across reconnect");
    assert_eq!(
        reconnected_output.state,
        CommandBlockOutputState::Incomplete(CommandBlockOutputIncompleteReason::Disconnected)
    );
    assert!(
        reconnected_output
            .text
            .as_ref()
            .is_some_and(|text| text.as_str().contains("output before disconnect\n"))
    );
    manager
        .close_all()
        .await
        .expect("close reconnected session");
}

/// 每条 session 只保留最新 128 个由真实 OSC C 观察到的块，并维持单调 ordinal。
///
/// @author kongweiguang
#[tokio::test]
async fn command_block_history_is_bounded_and_keeps_latest_protocol_ids() {
    let (manager, factory, session_id) = create_session(false).await;
    let mut output = factory.output.lock().await.take().expect("test PTY peer");
    send_markers(&mut output, b"\x1b]133;A\x07\x1b]133;B\x07").await;
    wait_for_prompt(manager.as_ref(), &session_id).await;

    send_markers(&mut output, b"\x1b]133;C\x07\x1b]133;D;0\x07").await;
    let first = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot.blocks.len() == 1
    })
    .await;
    let evicted_block_id = first.blocks[0].id;

    let mut markers = Vec::new();
    for _ in 0..(COMMAND_BLOCK_RETENTION_LIMIT + 1) {
        markers.extend_from_slice(b"\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07");
    }
    send_markers(&mut output, &markers).await;
    let snapshot = wait_for_snapshot(manager.as_ref(), &session_id, |snapshot| {
        snapshot
            .blocks
            .last()
            .is_some_and(|block| block.id.ordinal == (COMMAND_BLOCK_RETENTION_LIMIT + 2) as u64)
    })
    .await;
    assert_eq!(snapshot.blocks.len(), COMMAND_BLOCK_RETENTION_LIMIT);
    assert_eq!(snapshot.blocks.first().unwrap().id.ordinal, 3);
    assert!(snapshot.blocks.iter().all(|block| matches!(
        block.state,
        CommandBlockState::Finished { exit_code: Some(0) }
    )));
    let evicted_output = manager
        .command_block_output(
            session_id.clone(),
            evicted_block_id,
            MAX_COMMAND_BLOCK_OUTPUT_BYTES,
            &CancellationToken::new(),
        )
        .await
        .expect("evicted block has explicit query state");
    assert_eq!(evicted_output.state, CommandBlockOutputState::Evicted);

    manager.close_all().await.expect("close test session");
}
