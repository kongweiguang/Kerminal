//! Context inspector 的绑定筛选与来源状态回归。
//!
//! @author kongweiguang

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use kerminal_application::{
    AppResult,
    agents::{AgentResumeCapability, AgentSessionView},
    sessions::{
        CommandBlock, CommandBlockId, CommandBlockSnapshot, CommandBlockState, CommandBlockText,
        CommandBlockTextUnknownReason, CommandBlockUnknownReason, UiSessionBinding,
    },
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionStatus, AgentTargetBinding,
        AgentTargetLiveStatus,
    },
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    sessions::{SessionOwner, TerminalSize},
};
use tokio_util::sync::CancellationToken;

use crate::features::context::{
    ContextAgentAssociation, ContextArtifactIndex, ContextArtifactIndexState,
    ContextConnectionKind, ContextConnectionStatus, ContextLocationConfidence,
    ContextLocationSource, ContextLocationSummary, ContextPaneSummary, ContextProjectionStatus,
    ContextRuntimeSummary, ContextSourceStatus, ContextSourceSummary, ContextTabSummary,
    ContextTerminalLinkIndex, ContextWorkspaceProjection, associated_agent_summaries,
};

/// 构造完整业务模型和显式目标绑定；上下文投影不推断恢复能力，默认保持未核实。
///
/// @author kongweiguang
fn agent_view(binding: AgentTargetBinding, status: AgentSessionStatus) -> AgentSessionView {
    let mut session = AgentSession::new(
        AgentSessionId::new(),
        AgentProvider::Codex,
        None,
        "Review agent".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "workspace".to_owned(),
        "workspace/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "workspace/agents/session".to_owned(),
        },
        None,
        Some(binding),
    )
    .expect("valid Agent session fixture");
    session.status = status;
    AgentSessionView {
        session,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
        terminal: None,
        resume_recovery: None,
    }
}

/// 本机专用 ID 显示为本机名称，未知及远程标识保持原样而不猜测名称。
///
/// @author kongweiguang
#[test]
fn machine_display_localizes_only_the_known_local_identity() {
    let local = super::render::machine_display_label(super::render::LOCAL_MACHINE_ID).to_string();
    assert!(matches!(local.as_str(), "本机" | "Local machine"));
    assert_eq!(
        super::render::machine_display_label("machine-local-container").to_string(),
        "machine-local-container"
    );
    assert_eq!(
        super::render::machine_display_label("host-qa-01").to_string(),
        "host-qa-01"
    );
}

/// 同时固定活动 pane 与目标快照，避免测试依赖 Root 窗口或真实 transport。
fn projection(session_id: Option<SessionId>) -> ContextWorkspaceProjection {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    ContextWorkspaceProjection {
        status: ContextProjectionStatus::Ready,
        subject_title: "Workspace".to_owned(),
        machine_id: Some("machine-local".to_owned()),
        selected_host_id: None,
        tabs: vec![ContextTabSummary {
            tab_id: tab_id.clone(),
            title: "Terminal".to_owned(),
            kind: ContextConnectionKind::Terminal,
            machine_id: Some("machine-local".to_owned()),
            active: true,
        }],
        active_tab_id: Some(tab_id.clone()),
        active_target: None,
        location: ContextLocationSummary {
            path: None,
            source: ContextLocationSource::Unavailable,
            confidence: ContextLocationConfidence::Unknown,
        },
        active_pane: Some(ContextPaneSummary {
            pane_id,
            tab_id,
            session_id,
            title: "Terminal".to_owned(),
            kind: ContextConnectionKind::Local,
            host_id: None,
            target_ref: Some("local".to_owned()),
            status: ContextConnectionStatus::Ready,
            cwd: None,
            shell: None,
            columns: 120,
            rows: 36,
        }),
        runtime: ContextRuntimeSummary {
            tab_count: 1,
            pane_count: 1,
            ready_pane_count: 1,
            pending_pane_count: 0,
            failed_pane_count: 0,
            latency_ms: None,
            tmux_attached: None,
        },
        sources: vec![ContextSourceSummary {
            source: "Workspace".to_owned(),
            status: ContextSourceStatus::Available,
            updated_at: None,
        }],
        diagnostics: Vec::new(),
        terminal_links: ContextTerminalLinkIndex::not_connected(),
    }
}

/// 绑定必须命中同一个 live session，不因其它 Tab 或相似 target 而回退。
fn binding(
    pane_id: PaneId,
    tab_id: TabId,
    terminal_session_id: SessionId,
    live_status: AgentTargetLiveStatus,
) -> AgentTargetBinding {
    AgentTargetBinding {
        binding_id: Some("binding-1".to_owned()),
        generation: 1,
        pane_id: Some(pane_id),
        tab_id: Some(tab_id),
        terminal_session_id: Some(terminal_session_id),
        target_ref: Some("local".to_owned()),
        target_kind: Some("local".to_owned()),
        cwd: None,
        shell: None,
        live_status,
        last_seen_at: None,
    }
}

/// Ready 绑定只匹配明确 session/pane/tab；另一个 session 不会被当前 pane 冒领。
#[test]
fn ready_agent_matches_only_exact_live_session_binding() {
    let session_id = SessionId::new();
    let projection = projection(Some(session_id.clone()));
    let pane = projection.active_pane.as_ref().expect("active pane");
    let matching = agent_view(
        binding(
            pane.pane_id.clone(),
            pane.tab_id.clone(),
            session_id,
            AgentTargetLiveStatus::Ready,
        ),
        AgentSessionStatus::Active,
    );
    let unrelated = agent_view(
        binding(
            pane.pane_id.clone(),
            pane.tab_id.clone(),
            SessionId::new(),
            AgentTargetLiveStatus::Ready,
        ),
        AgentSessionStatus::Active,
    );

    let summaries = associated_agent_summaries(&projection, &[matching, unrelated]);

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].association, ContextAgentAssociation::Bound);
}

/// Stale 绑定只作为旧 pane 的诊断显示，CLI 生命周期仍可独立处于可继续状态。
#[test]
fn stale_agent_target_is_shown_without_rebinding_or_changing_lifecycle() {
    let projection = projection(Some(SessionId::new()));
    let pane = projection.active_pane.as_ref().expect("active pane");
    let stale = agent_view(
        binding(
            pane.pane_id.clone(),
            pane.tab_id.clone(),
            SessionId::new(),
            AgentTargetLiveStatus::Stale,
        ),
        AgentSessionStatus::Active,
    );

    let summaries = associated_agent_summaries(&projection, &[stale]);

    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].association,
        ContextAgentAssociation::StaleTarget
    );
    assert_eq!(
        summaries[0].lifecycle,
        crate::features::context::ContextAgentLifecycle::ReadyToResume
    );
}

/// 归档 Agent 与其它 pane 的绑定都从当前列表隐藏，不能因标题相同进入当前上下文。
#[test]
fn archived_and_other_tab_agents_are_not_associated() {
    let projection = projection(Some(SessionId::new()));
    let pane = projection.active_pane.as_ref().expect("active pane");
    let archived = agent_view(
        binding(
            pane.pane_id.clone(),
            pane.tab_id.clone(),
            pane.session_id.clone().expect("live session"),
            AgentTargetLiveStatus::Ready,
        ),
        AgentSessionStatus::Archived,
    );
    let other_tab = agent_view(
        binding(
            pane.pane_id.clone(),
            TabId::new(),
            pane.session_id.clone().expect("live session"),
            AgentTargetLiveStatus::Stale,
        ),
        AgentSessionStatus::Active,
    );

    assert!(associated_agent_summaries(&projection, &[archived, other_tab]).is_empty());
}

/// 未接入索引与已读取后的真实空结果必须保持不同状态。
#[test]
fn discovery_index_distinguishes_unconnected_from_available_empty() {
    let mut projection = ContextWorkspaceProjection::unavailable();
    assert_eq!(
        projection.terminal_links.state,
        ContextArtifactIndexState::NotConnected
    );

    projection.terminal_links.state = ContextArtifactIndexState::Available;
    assert_eq!(
        projection.terminal_links.state,
        ContextArtifactIndexState::Available
    );
    assert!(projection.terminal_links.entries.is_empty());
}

/// 旧 viewport 的部分结果仍标记过期，不能以 partial 为由恢复点击能力。
#[test]
fn stale_link_snapshot_state_takes_precedence_over_partial_scan() {
    assert_eq!(
        super::projection::terminal_link_state(false, true),
        ContextArtifactIndexState::Stale
    );
    assert_eq!(
        super::projection::terminal_link_state(true, true),
        ContextArtifactIndexState::Partial
    );
}

/// Link rows remain actionable only for the exact UI owner and terminal viewport that produced them.
#[test]
fn terminal_link_identity_requires_binding_revision_scroll_and_size_match() {
    let session_id = SessionId::new();
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 7,
    };
    let size = TerminalSize::new(120, 36, 0, 0).expect("valid viewport size");
    let identity = crate::terminal_canvas::TerminalLinkSnapshotIdentity {
        binding: binding.clone(),
        revision: 42,
        scrollback_offset: 5,
        size,
    };

    assert!(super::terminal_link_identity_matches(
        &identity,
        &binding,
        &session_id,
        42,
        5,
        size,
    ));
    assert!(!super::terminal_link_identity_matches(
        &identity,
        &binding,
        &session_id,
        43,
        5,
        size,
    ));
    assert!(!super::terminal_link_identity_matches(
        &identity,
        &binding,
        &session_id,
        42,
        6,
        size,
    ));
    let changed_size = TerminalSize::new(100, 36, 0, 0).expect("valid viewport size");
    assert!(!super::terminal_link_identity_matches(
        &identity,
        &binding,
        &session_id,
        42,
        5,
        changed_size,
    ));
    let changed_generation = UiSessionBinding {
        session_generation: 8,
        ..binding.clone()
    };
    assert!(!super::terminal_link_identity_matches(
        &identity,
        &changed_generation,
        &session_id,
        42,
        5,
        size,
    ));
}

/// 链接和命令块保留独立来源状态，刷新其中一侧不会覆盖另一侧的结果。
#[test]
fn link_and_command_discovery_states_are_independent() {
    let mut projection = ContextWorkspaceProjection::unavailable();
    projection.terminal_links.state = ContextArtifactIndexState::Available;
    let command_artifacts = ContextArtifactIndex {
        state: ContextArtifactIndexState::Available,
        entries: Vec::new(),
    };
    assert_eq!(
        projection.terminal_links.state,
        ContextArtifactIndexState::Available
    );
    assert_eq!(
        command_artifacts.state,
        ContextArtifactIndexState::Available
    );
    assert!(projection.terminal_links.entries.is_empty());
    assert!(command_artifacts.entries.is_empty());
}

/// OSC 133 只投影块状态和退出码，已捕获命令正文也不会进入 Context 标签。
#[test]
fn command_block_projection_never_exposes_command_text() {
    let snapshot = CommandBlockSnapshot {
        session_id: SessionId::new(),
        session_generation: 9,
        revision: 12,
        blocks: vec![
            CommandBlock {
                id: CommandBlockId {
                    session_generation: 9,
                    prompt_epoch: 3,
                    ordinal: 1,
                },
                command: CommandBlockText::Known("token=never-render-this".to_owned()),
                state: CommandBlockState::Finished { exit_code: Some(0) },
            },
            CommandBlock {
                id: CommandBlockId {
                    session_generation: 9,
                    prompt_epoch: 4,
                    ordinal: 1,
                },
                command: CommandBlockText::Unknown(
                    CommandBlockTextUnknownReason::SensitiveOrUnsafe,
                ),
                state: CommandBlockState::Unknown(CommandBlockUnknownReason::Disconnected),
            },
        ],
    };

    let projected = super::projection::command_block_artifact_index(&snapshot);

    assert_eq!(projected.state, ContextArtifactIndexState::Partial);
    assert_eq!(projected.entries.len(), 2);
    assert!(projected.entries[0].title.contains("退出码 0"));
    assert!(
        projected
            .entries
            .iter()
            .all(|entry| !entry.title.contains("token=never-render-this"))
    );
}

struct WaitForCancellation {
    cancellation: CancellationToken,
    dropped: Arc<AtomicBool>,
}

impl Future for WaitForCancellation {
    type Output = AppResult<Vec<AgentSessionView>>;

    /// 仅在应用服务确认协作取消后结束，模拟仍有自身收尾工作的列表查询。
    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.cancellation.is_cancelled() {
            Poll::Ready(Ok(Vec::new()))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for WaitForCancellation {
    /// 标记原查询 future 最终释放，确认 UI 等待桥没有提前 drop 它。
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// 手动推进一次 future，不引入 Tokio runtime 或其它测试执行器。
fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// UI 隐藏触发协作取消后，仍等待服务查询结束并完成其自身清理。
#[test]
fn hiding_context_waits_for_agent_query_cleanup() {
    let service_cancellation = CancellationToken::new();
    let panel_cancellation = CancellationToken::new();
    let executor_cancellation = CancellationToken::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let operation = WaitForCancellation {
        cancellation: service_cancellation.clone(),
        dropped: Arc::clone(&dropped),
    };
    let mut waiting = Box::pin(super::await_agent_list_with_cancellation(
        operation,
        service_cancellation.clone(),
        panel_cancellation.clone(),
        executor_cancellation,
    ));

    assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
    panel_cancellation.cancel();
    assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
    assert!(service_cancellation.is_cancelled());
    assert!(!dropped.load(Ordering::SeqCst));

    assert!(matches!(poll_once(waiting.as_mut()), Poll::Ready(Ok(views)) if views.is_empty()));
    assert!(dropped.load(Ordering::SeqCst));
}

/// 应用 executor 停止时同样通知查询取消，并保留其 future 到 cleanup acknowledgement。
#[test]
fn executor_shutdown_waits_for_agent_query_cleanup() {
    let service_cancellation = CancellationToken::new();
    let panel_cancellation = CancellationToken::new();
    let executor_cancellation = CancellationToken::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let operation = WaitForCancellation {
        cancellation: service_cancellation.clone(),
        dropped: Arc::clone(&dropped),
    };
    let mut waiting = Box::pin(super::await_agent_list_with_cancellation(
        operation,
        service_cancellation.clone(),
        panel_cancellation,
        executor_cancellation.clone(),
    ));

    assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
    executor_cancellation.cancel();
    assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
    assert!(service_cancellation.is_cancelled());
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(matches!(poll_once(waiting.as_mut()), Poll::Ready(Ok(views)) if views.is_empty()));
    assert!(dropped.load(Ordering::SeqCst));
}

/// 查询先完成时保留真实结果，不被随后已经置位的取消信号覆盖。
#[test]
fn completed_agent_query_wins_before_cancellation() {
    let service_cancellation = CancellationToken::new();
    let panel_cancellation = CancellationToken::new();
    panel_cancellation.cancel();
    let mut waiting = Box::pin(super::await_agent_list_with_cancellation(
        async { Ok(Vec::new()) },
        service_cancellation.clone(),
        panel_cancellation,
        CancellationToken::new(),
    ));

    assert!(matches!(poll_once(waiting.as_mut()), Poll::Ready(Ok(views)) if views.is_empty()));
    assert!(!service_cancellation.is_cancelled());
}
