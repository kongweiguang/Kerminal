//! Workspace runtime 的 pane/session 映射与关闭状态契约。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, sync::Arc};

use gpui_kit::SharedString;
use kerminal_application::{
    sessions::UiSessionBinding,
    workspace::{
        WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionOwner, SessionTarget, TerminalSize},
    workspace::{SplitDirection, TerminalLayoutNode},
};
use kerminal_terminal::snapshot::{
    RenderSnapshot, TerminalCursor, TerminalCursorShape, TerminalModes, TerminalPalette,
    TerminalRow,
};

use crate::{
    features::workspace_controller::{WorkspaceController, WorkspacePersistenceState},
    root::{
        workspace_runtime::{
            OrphanCleanupInput, RuntimePanePhase, WorkspaceRuntime, WorkspaceRuntimeError,
        },
        workspace_transient::without_transient_resources,
    },
};

fn pane(id: PaneId) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id,
        title: "shell".to_owned(),
        machine_id: "local".to_owned(),
        mode: WorkspacePaneMode::Local,
        target: Some(WorkspaceTargetRef::Local { profile_id: None }),
        remote_host_id: None,
        container_id: None,
        profile_id: None,
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Online,
        latency_ms: None,
        local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
        output_history: None,
    }
}

fn snapshot(pane: WorkspaceSessionPane, tab_id: TabId) -> WorkspaceSessionSnapshot {
    WorkspaceSessionSnapshot {
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane.id.clone()),
        selected_machine_id: None,
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id,
            tab_group_id: None,
            title: "Local".to_owned(),
            machine_id: "local".to_owned(),
            layout: TerminalLayoutNode::pane(pane.id.clone()),
        }],
        terminal_panes: vec![pane],
        tab_groups: BTreeMap::new(),
    }
}

fn local_target() -> SessionTarget {
    SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("configured-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    })
}

/// 构造固定坐标世代且不含目录信号的轻量快照；各目录测试显式设置元数据，避免隐式共享状态。
///
/// @author kongweiguang
fn render_snapshot(
    session_id: SessionId,
    size: TerminalSize,
    scrollback_offset: usize,
    revision: u64,
) -> RenderSnapshot {
    RenderSnapshot {
        session_id,
        revision,
        size,
        scrollback_offset,
        scroll_origin: 0,
        grid_epoch: 0,
        history_size: scrollback_offset,
        title: None,
        current_working_directory: None,
        cursor: TerminalCursor {
            row: 0,
            column: 0,
            shape: TerminalCursorShape::Block,
            style_from_terminal: false,
            blinking: false,
            visible: false,
        },
        cursor_cell: None,
        modes: TerminalModes::default(),
        palette: TerminalPalette::default(),
        rows: Arc::from(Vec::<TerminalRow>::new()),
        dirty_rows: Arc::from(Vec::<u16>::new()),
        selection: None,
        search_matches: Arc::from([]),
    }
}

/// v3 pane 先作为待恢复项展示，只有应用确认真实创建后才绑定 SessionId。
///
/// @author kongweiguang
#[test]
fn restore_projection_keeps_pending_pane_without_fallback_session() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let persisted = pane(pane_id.clone());
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(persisted.clone(), tab_id.clone()))
        .expect("valid v3 snapshot");

    assert_eq!(runtime.session_for_pane(&pane_id), None);
    assert_eq!(runtime.panes().count(), 1);
    assert_eq!(
        runtime.panes().next().unwrap().1.phase,
        RuntimePanePhase::RestorePending
    );
    let (runtime_pane_id, runtime_pane) = runtime.panes().next().unwrap();
    assert_eq!(runtime_pane_id, &pane_id);
    assert_eq!(runtime_pane.tab_id, tab_id);
    assert_eq!(runtime_pane.persisted, persisted);

    let session_id = SessionId::new();
    let size = TerminalSize::new(100, 30, 1000, 600).unwrap();
    runtime
        .install_session(&pane_id, session_id.clone(), local_target(), size)
        .expect("actual session creation binds the pane");
    assert_eq!(runtime.session_for_pane(&pane_id), Some(&session_id));
    assert_eq!(runtime.pane_for_session(&session_id), Some(&pane_id));
    assert_eq!(runtime.panes().next().unwrap().1.size, size);
    assert_eq!(
        runtime.panes().next().unwrap().1.phase,
        RuntimePanePhase::Ready
    );
}

/// 创建错误保持原 PaneId/target 并允许同一意图重试，不会生成替代 session。
#[test]
fn restore_failure_remains_retryable_and_duplicate_session_is_rejected() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id))
        .expect("valid v3 snapshot");
    runtime.mark_restore_failed(&pane_id).unwrap();
    assert_eq!(runtime.session_for_pane(&pane_id), None);
    assert_eq!(
        runtime.panes().next().unwrap().1.phase,
        RuntimePanePhase::RestoreFailed
    );

    let session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .unwrap();
    assert_eq!(
        runtime.install_session(
            &pane_id,
            session_id,
            local_target(),
            TerminalSize::default()
        ),
        Err(WorkspaceRuntimeError::DuplicateSession)
    );
}

/// 只有精确失败空 pane 可重新进入首尺寸测量，运行态拒绝目标漂移、其他阶段和残留 session。
///
/// @author kongweiguang
#[test]
fn failed_split_rearm_requires_exact_owner_and_empty_restore_failure() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let persisted = pane(pane_id.clone());
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(persisted.clone(), tab_id.clone()))
        .expect("valid v3 snapshot");
    runtime.mark_restore_failed(&pane_id).unwrap();
    runtime
        .set_restore_target(&pane_id, local_target())
        .unwrap();
    runtime
        .set_status(
            &pane_id,
            Some(SharedString::from("previous restore failed")),
        )
        .unwrap();

    let foreign_tab = TabId::new();
    assert_eq!(
        runtime.rearm_failed_split_for_measurement(&foreign_tab, &persisted),
        Err(WorkspaceRuntimeError::PaneNotRestorable)
    );
    let mut foreign_host = persisted.clone();
    foreign_host.remote_host_id = Some(HostId::new());
    assert_eq!(
        runtime.rearm_failed_split_for_measurement(&tab_id, &foreign_host),
        Err(WorkspaceRuntimeError::PaneNotRestorable),
        "host metadata is part of the pane owner identity"
    );
    assert_eq!(
        runtime.rearm_failed_split_for_measurement(&tab_id, &persisted),
        Ok(())
    );
    let pane = runtime.pane(&pane_id).unwrap();
    assert_eq!(pane.phase, RuntimePanePhase::RestorePending);
    assert_eq!(pane.session_id, None);
    assert_eq!(pane.status, None);
    assert_eq!(pane.persisted, persisted);
    assert!(pane.target.is_some(), "retry preserves the resolved target");

    assert_eq!(
        runtime.rearm_failed_split_for_measurement(&tab_id, &persisted),
        Err(WorkspaceRuntimeError::PaneNotRestorable),
        "a duplicate retry cannot reuse the failed-state transition"
    );
}

/// 非失败阶段、带活动 session 或 pane ID 不匹配时不得修改运行态重试阶段。
///
/// @author kongweiguang
#[test]
fn failed_split_rearm_rejects_other_phase_session_and_pane_identity() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let persisted = pane(pane_id.clone());
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(persisted.clone(), tab_id.clone()))
        .expect("valid v3 snapshot");

    assert_eq!(
        runtime.rearm_failed_split_for_measurement(&tab_id, &persisted),
        Err(WorkspaceRuntimeError::PaneNotRestorable),
        "RestorePending is not a failed retry"
    );
    let mut foreign_pane = persisted.clone();
    foreign_pane.id = PaneId::new();
    assert_eq!(
        runtime.rearm_failed_split_for_measurement(&tab_id, &foreign_pane),
        Err(WorkspaceRuntimeError::PaneMissing)
    );

    runtime.mark_restore_failed(&pane_id).unwrap();
    let pane = runtime.panes.get_mut(&pane_id).unwrap();
    pane.session_id = Some(SessionId::new());
    assert_eq!(
        runtime.rearm_failed_split_for_measurement(&tab_id, &persisted),
        Err(WorkspaceRuntimeError::PaneNotRestorable),
        "a failed phase with a residual session is not safe to restart"
    );
    assert_eq!(
        runtime.pane(&pane_id).unwrap().phase,
        RuntimePanePhase::RestoreFailed
    );
}

/// 关闭请求可见为 in-progress，失败保留可重试资源，成功后才删除运行态映射。
#[test]
fn close_failure_retains_session_and_close_success_removes_it() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id))
        .expect("valid v3 snapshot");
    let session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .unwrap();
    assert_eq!(runtime.begin_close(&pane_id), Ok(session_id.clone()));
    assert!(
        !runtime
            .mark_close_failed(&pane_id, &SessionId::new())
            .unwrap()
    );
    assert!(runtime.mark_close_failed(&pane_id, &session_id).unwrap());
    assert_eq!(runtime.session_for_pane(&pane_id), Some(&session_id));
    assert_eq!(
        runtime.panes().next().unwrap().1.phase,
        RuntimePanePhase::CloseFailed
    );

    runtime.begin_close(&pane_id).unwrap();
    assert!(
        runtime
            .remove_closed_session(&pane_id, &session_id)
            .unwrap()
    );
    assert_eq!(runtime.session_for_pane(&pane_id), None);
    assert!(runtime.panes().next().is_none());
}

/// CloseFailed history remains readable only under the naturally retired exact binding and a fresh snapshot token.
///
/// @author kongweiguang
#[test]
fn close_failed_history_requires_exact_retired_binding_and_rejects_preclose_snapshot() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime =
        WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id.clone()))
            .expect("valid workspace snapshot");
    let session_id = SessionId::new();
    let size = TerminalSize::new(80, 24, 0, 0).expect("valid grid");
    runtime
        .install_session(&pane_id, session_id.clone(), local_target(), size)
        .expect("install live session");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: SessionOwner::Ui {
            tab_id,
            pane_id: pane_id.clone(),
        },
        session_generation: 9,
    };
    assert!(runtime.retire_history_binding(&pane_id, binding.clone()));

    let (_, old_token, _) = runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    runtime.begin_close(&pane_id).unwrap();
    assert!(!runtime.close_failed_history_binding_matches(&pane_id, &binding));
    assert!(runtime.mark_close_failed(&pane_id, &session_id).unwrap());
    assert!(runtime.close_failed_history_binding_matches(&pane_id, &binding));
    let mut wrong_generation = binding.clone();
    wrong_generation.session_generation += 1;
    assert!(!runtime.close_failed_history_binding_matches(&pane_id, &wrong_generation));

    let (_, token, _) = runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    assert_ne!(
        old_token, token,
        "close invalidates the earlier snapshot generation"
    );
    assert!(
        !runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                old_token,
                render_snapshot(session_id.clone(), size, 0, 2),
            )
            .unwrap()
    );
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                token,
                render_snapshot(session_id.clone(), size, 0, 3),
            )
            .unwrap()
    );

    runtime
        .finish_snapshot_request(&pane_id, &session_id, token, true, false)
        .unwrap();
    let view_resize = TerminalSize::new(80, 25, 0, 0).expect("valid history view grid");
    assert!(
        runtime
            .history_view_resize_needs_enqueue(&pane_id, view_resize)
            .expect("exact retired ticket admits history-only grid changes")
    );
    runtime
        .mark_history_view_resize_pending(&pane_id, view_resize)
        .expect("history resize tracks a separate view ACK");
    assert!(
        runtime
            .complete_history_view_resize(&pane_id, &session_id, view_resize, true)
            .expect("history view ACK is accepted after close failure")
    );
    assert_eq!(runtime.pane(&pane_id).unwrap().view_size, view_resize);
    assert!(runtime.snapshot_needs_refresh(&pane_id));
    assert!(runtime.resize_needs_enqueue(&pane_id, view_resize).is_err());
}

/// CloseFailed without a naturally retired exact view ticket remains inaccessible to snapshots and history resize.
///
/// @author kongweiguang
#[test]
fn close_failed_without_retired_ticket_rejects_history_view_operations() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime =
        WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id)).unwrap();
    let session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .unwrap();
    runtime.begin_close(&pane_id).unwrap();
    assert!(runtime.mark_close_failed(&pane_id, &session_id).unwrap());

    assert!(!runtime.snapshot_needs_refresh(&pane_id));
    assert!(runtime.begin_snapshot(&pane_id).is_err());
    assert!(
        runtime
            .history_view_resize_needs_enqueue(&pane_id, TerminalSize::default())
            .is_err()
    );
    assert!(
        runtime
            .mark_history_view_resize_pending(&pane_id, TerminalSize::default())
            .is_err()
    );
}

/// 同一运行态 session 不能在多个 pane 中重复绑定，以免一次输入被意外复制。
#[test]
fn duplicate_pane_id_and_duplicate_session_binding_are_rejected() {
    let first_pane = PaneId::new();
    let first_tab = TabId::new();
    let mut runtime =
        WorkspaceRuntime::from_snapshot(&snapshot(pane(first_pane.clone()), first_tab))
            .expect("valid v3 snapshot");
    assert_eq!(
        runtime.register_pending_pane(TabId::new(), pane(first_pane.clone())),
        Err(WorkspaceRuntimeError::DuplicatePane)
    );
    let second_pane = PaneId::new();
    runtime
        .register_pending_pane(TabId::new(), pane(second_pane.clone()))
        .unwrap();
    let session_id = SessionId::new();
    runtime
        .install_session(
            &first_pane,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .unwrap();
    assert_eq!(
        runtime.install_session(
            &second_pane,
            session_id,
            local_target(),
            TerminalSize::default()
        ),
        Err(WorkspaceRuntimeError::DuplicateSession)
    );
}

/// 目录与已接受快照同源；resize 不使它回退，异 session/迟到代次也不能污染它或持久目标。
///
/// @author kongweiguang
#[test]
fn live_directory_tracks_only_accepted_session_snapshots_and_survives_resize() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let persisted = pane(pane_id.clone());
    let mut runtime =
        WorkspaceRuntime::from_snapshot(&snapshot(persisted.clone(), tab_id)).unwrap();
    let session_id = SessionId::new();
    let size = TerminalSize::default();
    runtime
        .install_session(&pane_id, session_id.clone(), local_target(), size)
        .unwrap();
    let mut initial = render_snapshot(session_id.clone(), size, 0, 1);
    initial.current_working_directory = Some(Arc::from("/home/验收 a"));
    assert!(
        runtime
            .accept_initial_snapshot(&pane_id, &session_id, initial)
            .unwrap()
    );
    let resized = TerminalSize::new(100, 30, 1000, 600).unwrap();
    runtime
        .complete_resize(&pane_id, &session_id, resized, true)
        .unwrap();
    let pane = runtime.pane(&pane_id).unwrap();
    assert!(pane.snapshot.is_none());
    assert_eq!(
        pane.current_working_directory.as_deref(),
        Some("/home/验收 a")
    );
    assert_eq!(pane.persisted, persisted);

    let (_, token, _) = runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    let mut foreign = render_snapshot(SessionId::new(), resized, 0, 2);
    foreign.current_working_directory = Some(Arc::from("/foreign"));
    assert!(
        !runtime
            .accept_snapshot(&pane_id, &session_id, token, foreign)
            .unwrap()
    );
    let mut latest = render_snapshot(session_id.clone(), resized, 0, 2);
    latest.current_working_directory = Some(Arc::from("/home/next"));
    assert!(
        runtime
            .accept_snapshot(&pane_id, &session_id, token, latest)
            .unwrap()
    );
    let mut stale = render_snapshot(session_id.clone(), resized, 0, 1);
    stale.current_working_directory = Some(Arc::from("/stale"));
    assert!(
        !runtime
            .accept_snapshot(&pane_id, &session_id, token, stale)
            .unwrap()
    );
    let pane = runtime.pane(&pane_id).unwrap();
    assert_eq!(
        pane.current_working_directory.as_deref(),
        Some("/home/next")
    );
    assert_eq!(pane.persisted, persisted);
}

/// Resize 失败保留确认网格并允许同尺寸重试；成功 ACK 后才使旧快照失效。
#[test]
fn resize_size_changes_only_after_ack_and_failed_size_can_retry() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id))
        .expect("valid v3 snapshot");
    let session_id = SessionId::new();
    let confirmed = TerminalSize::new(80, 24, 800, 480).unwrap();
    let requested = TerminalSize::new(100, 30, 1000, 600).unwrap();
    runtime
        .install_session(&pane_id, session_id.clone(), local_target(), confirmed)
        .unwrap();

    assert!(runtime.resize_needs_enqueue(&pane_id, requested).unwrap());
    runtime.mark_resize_pending(&pane_id, requested).unwrap();
    assert!(!runtime.resize_needs_enqueue(&pane_id, requested).unwrap());
    assert!(
        runtime
            .complete_resize(&pane_id, &session_id, requested, false)
            .unwrap()
    );
    let pane = runtime.pane(&pane_id).unwrap();
    assert_eq!(pane.size, confirmed);
    assert_eq!(pane.pending_resize, None);
    assert!(runtime.resize_needs_enqueue(&pane_id, requested).unwrap());

    runtime.mark_resize_pending(&pane_id, requested).unwrap();
    assert!(
        runtime
            .complete_resize(&pane_id, &session_id, requested, true)
            .unwrap()
    );
    let pane = runtime.pane(&pane_id).unwrap();
    assert_eq!(pane.size, requested);
    assert_eq!(pane.pending_resize, None);
}

/// Exited 历史只更新 engine view 网格；不得推进 PTY 确认尺寸或持久化 pane。
///
/// @author kongweiguang
#[test]
fn history_view_resize_reflows_snapshot_grid_without_claiming_transport_ack() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let persisted = pane(pane_id.clone());
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(persisted.clone(), tab_id))
        .expect("valid v3 snapshot");
    let session_id = SessionId::new();
    let confirmed = TerminalSize::new(80, 24, 800, 480).unwrap();
    let historical = TerminalSize::new(120, 36, 1200, 720).unwrap();
    runtime
        .install_session(&pane_id, session_id.clone(), local_target(), confirmed)
        .unwrap();
    runtime.mark_resize_pending(&pane_id, historical).unwrap();
    assert!(
        runtime
            .clear_pending_transport_resize_after_exit(&pane_id, &session_id)
            .unwrap()
    );
    assert_eq!(runtime.pane(&pane_id).unwrap().pending_resize, None);
    assert_eq!(runtime.pane(&pane_id).unwrap().size, confirmed);
    assert!(
        runtime
            .accept_initial_snapshot(
                &pane_id,
                &session_id,
                render_snapshot(session_id.clone(), confirmed, 0, 1),
            )
            .unwrap()
    );

    assert!(
        runtime
            .history_view_resize_needs_enqueue(&pane_id, historical)
            .unwrap()
    );
    runtime
        .mark_history_view_resize_pending(&pane_id, historical)
        .unwrap();
    assert!(
        runtime
            .reset_pending_history_view_resize_after_queue_cancel(
                &pane_id,
                &session_id,
                Some(confirmed),
            )
            .unwrap()
    );
    assert_eq!(
        runtime.pane(&pane_id).unwrap().pending_history_view_resize,
        Some(confirmed)
    );
    assert!(
        runtime
            .reset_pending_history_view_resize_after_queue_cancel(&pane_id, &session_id, None)
            .unwrap()
    );
    assert_eq!(
        runtime.pane(&pane_id).unwrap().pending_history_view_resize,
        None
    );

    assert!(
        runtime
            .history_view_resize_needs_enqueue(&pane_id, historical)
            .unwrap()
    );
    runtime
        .mark_history_view_resize_pending(&pane_id, historical)
        .unwrap();
    assert!(
        runtime
            .complete_history_view_resize(&pane_id, &session_id, historical, false)
            .unwrap()
    );
    let pane_state = runtime.pane(&pane_id).unwrap();
    assert_eq!(pane_state.size, confirmed);
    assert_eq!(pane_state.view_size, confirmed);
    assert_eq!(pane_state.pending_history_view_resize, None);
    assert!(
        runtime
            .history_view_resize_needs_enqueue(&pane_id, historical)
            .unwrap()
    );

    runtime
        .mark_history_view_resize_pending(&pane_id, historical)
        .unwrap();
    assert!(
        runtime
            .complete_history_view_resize(&pane_id, &session_id, historical, true)
            .unwrap()
    );
    let pane_state = runtime.pane(&pane_id).unwrap();
    assert_eq!(pane_state.size, confirmed);
    assert_eq!(pane_state.view_size, historical);
    assert_eq!(pane_state.pending_history_view_resize, None);
    assert!(pane_state.snapshot.is_none());
    assert_eq!(pane_state.persisted, persisted);

    let (_, token, _) = runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                token,
                render_snapshot(session_id.clone(), historical, 0, 2),
            )
            .unwrap()
    );
}

/// 输出通知只合并成一次后续读取，滚动代次失效后旧 viewport 回执不能覆盖引擎的新位置。
///
/// @author kongweiguang
#[test]
fn snapshot_requests_are_single_flight_and_viewport_changes_reject_stale_frames() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id))
        .expect("valid v3 snapshot");
    let session_id = SessionId::new();
    let size = TerminalSize::new(80, 24, 800, 480).unwrap();
    runtime
        .install_session(&pane_id, session_id.clone(), local_target(), size)
        .unwrap();
    assert!(runtime.snapshot_needs_refresh(&pane_id));

    let (first_session, first_token, _) = runtime
        .begin_snapshot(&pane_id)
        .unwrap()
        .expect("first visible refresh starts one request");
    assert_eq!(first_session, session_id);
    assert!(!runtime.snapshot_needs_refresh(&pane_id));
    assert!(runtime.begin_snapshot(&pane_id).unwrap().is_none());
    assert!(runtime.pane(&pane_id).unwrap().snapshot_dirty);
    assert!(
        runtime
            .invalidate_snapshot_for_viewport_change(&pane_id, &session_id)
            .unwrap()
    );

    assert!(
        !runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                first_token,
                render_snapshot(session_id.clone(), size, 0, 1),
            )
            .unwrap()
    );
    assert!(
        runtime
            .finish_snapshot_request(&pane_id, &session_id, first_token, false, true)
            .unwrap()
    );

    let (next_session, next_token, request) = runtime
        .begin_snapshot(&pane_id)
        .unwrap()
        .expect("coalesced refresh starts after the first request completes");
    assert_eq!(next_session, session_id);
    assert_eq!(request.since_revision, None);
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                next_token,
                render_snapshot(session_id.clone(), size, 19, 2),
            )
            .unwrap()
    );
    assert!(
        !runtime
            .finish_snapshot_request(&pane_id, &session_id, next_token, true, true)
            .unwrap()
    );
    assert!(!runtime.pane(&pane_id).unwrap().snapshot_dirty);
    assert!(!runtime.snapshot_needs_refresh(&pane_id));
}

/// 输出、滚动和 resize 只刷新引擎快照；UI 不携带或重放 viewport offset。
///
/// @author kongweiguang
#[test]
fn engine_viewport_snapshot_survives_scroll_refresh_and_resize() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let persisted = pane(pane_id.clone());
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(persisted.clone(), tab_id.clone()))
        .expect("valid v3 snapshot");
    let session_id = SessionId::new();
    let original_size = TerminalSize::new(80, 24, 800, 480).unwrap();
    let resized_size = TerminalSize::new(100, 30, 1000, 600).unwrap();
    runtime
        .install_session(&pane_id, session_id.clone(), local_target(), original_size)
        .unwrap();

    let (_, initial_token, initial_request) = runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    assert_eq!(initial_request.since_revision, None);
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                initial_token,
                render_snapshot(session_id.clone(), original_size, 7, 1),
            )
            .unwrap()
    );
    assert!(
        !runtime
            .finish_snapshot_request(&pane_id, &session_id, initial_token, true, true)
            .unwrap()
    );

    assert!(
        runtime
            .invalidate_snapshot_for_viewport_change(&pane_id, &session_id)
            .unwrap()
    );
    let (_, viewport_token, viewport_request) = runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    assert_eq!(viewport_request.since_revision, Some(1));
    assert!(runtime.begin_snapshot(&pane_id).unwrap().is_none());
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                viewport_token,
                render_snapshot(session_id.clone(), original_size, 11, 2),
            )
            .unwrap()
    );
    assert!(
        runtime
            .finish_snapshot_request(&pane_id, &session_id, viewport_token, true, true)
            .unwrap(),
        "output during a viewport request is merged into one follow-up read"
    );
    let (_, viewport_follow_up_token, follow_up_request) =
        runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    assert_eq!(follow_up_request.since_revision, Some(2));
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                viewport_follow_up_token,
                render_snapshot(session_id.clone(), original_size, 11, 3),
            )
            .unwrap()
    );
    assert!(
        !runtime
            .finish_snapshot_request(&pane_id, &session_id, viewport_follow_up_token, true, true)
            .unwrap()
    );

    runtime.mark_resize_pending(&pane_id, resized_size).unwrap();
    assert!(
        runtime
            .complete_resize(&pane_id, &session_id, resized_size, true)
            .unwrap()
    );
    assert!(runtime.pane(&pane_id).unwrap().snapshot.is_none());
    assert!(
        runtime.snapshot_session_is_current(&pane_id, &session_id),
        "resize keeps the watcher bound to the same session"
    );
    assert!(
        !runtime.snapshot_session_is_current(&pane_id, &SessionId::new()),
        "a stale session event must not keep a replacement watcher alive"
    );

    let (started_session, resize_token, request) = runtime
        .begin_snapshot(&pane_id)
        .unwrap()
        .expect("resize refresh starts for the still-ready session");
    assert_eq!(started_session, session_id);
    assert_eq!(request.since_revision, Some(3));

    // Palette changes are delivered through the same Output refresh path; while this read is
    // outstanding, another output notification must coalesce instead of ending the watcher.
    assert!(runtime.begin_snapshot(&pane_id).unwrap().is_none());
    assert!(runtime.pane(&pane_id).unwrap().snapshot_dirty);
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                resize_token,
                render_snapshot(session_id.clone(), resized_size, 11, 4),
            )
            .unwrap()
    );
    assert!(
        runtime
            .finish_snapshot_request(&pane_id, &session_id, resize_token, true, true)
            .unwrap(),
        "coalesced output requires a follow-up read after the resize snapshot"
    );
    let (_, follow_up_token, follow_up_request) = runtime
        .begin_snapshot(&pane_id)
        .unwrap()
        .expect("single-flight completion schedules the merged refresh");
    assert_eq!(follow_up_request.since_revision, Some(4));
    assert!(
        runtime
            .accept_snapshot(
                &pane_id,
                &session_id,
                follow_up_token,
                render_snapshot(session_id.clone(), resized_size, 11, 5),
            )
            .unwrap()
    );
    assert!(
        !runtime
            .finish_snapshot_request(&pane_id, &session_id, follow_up_token, true, true)
            .unwrap()
    );

    runtime.begin_close(&pane_id).unwrap();
    assert!(!runtime.snapshot_session_is_current(&pane_id, &session_id));
    assert!(
        runtime
            .remove_closed_session(&pane_id, &session_id)
            .unwrap()
    );
    runtime
        .register_pending_pane(tab_id, persisted)
        .expect("the same pane identity can be rebound");
    let replacement_session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            replacement_session_id.clone(),
            local_target(),
            resized_size,
        )
        .unwrap();
    assert!(!runtime.snapshot_session_is_current(&pane_id, &session_id));
    assert!(runtime.snapshot_session_is_current(&pane_id, &replacement_session_id));
}

/// 初始快照后唯一一次输出可能早于 binding；dirty 必须保留至绑定，失败也不能产生并发或热循环。
///
/// @author kongweiguang
#[test]
fn deferred_and_failed_snapshot_remains_retryable_without_parallel_rpc() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime =
        WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id.clone()))
            .expect("valid v3 snapshot");
    let session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .unwrap();

    runtime
        .accept_initial_snapshot(
            &pane_id,
            &session_id,
            render_snapshot(session_id.clone(), TerminalSize::default(), 0, 1),
        )
        .unwrap();
    assert!(!runtime.snapshot_needs_refresh(&pane_id));
    assert!(!runtime.prepare_bound_snapshot(&tab_id, &pane_id, None));
    assert!(runtime.pane(&pane_id).unwrap().snapshot_in_flight.is_none());
    assert!(runtime.pane(&pane_id).unwrap().snapshot_dirty);
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
        session_generation: 1,
    };
    let mut wrong_binding = binding.clone();
    wrong_binding.session_id = SessionId::new();
    assert!(!runtime.prepare_bound_snapshot(&tab_id, &pane_id, Some(&wrong_binding)));
    assert!(runtime.prepare_bound_snapshot(&tab_id, &pane_id, Some(&binding)));
    assert!(runtime.snapshot_needs_refresh(&pane_id));
    let (started_session, token, request) = runtime
        .begin_snapshot(&pane_id)
        .unwrap()
        .expect("becoming visible starts the deferred refresh");
    assert_eq!(started_session, session_id);
    assert_eq!(request.since_revision, Some(1));

    runtime
        .mark_snapshot_failed(&pane_id, &session_id, token)
        .unwrap();
    assert!(
        !runtime
            .finish_snapshot_request(&pane_id, &session_id, token, false, false)
            .unwrap(),
        "failed RPC is not retried in a hot loop"
    );
    assert!(runtime.pane(&pane_id).unwrap().snapshot_dirty);
    let (_, rebound_token, _) = runtime.begin_snapshot(&pane_id).unwrap().unwrap();
    assert!(
        !runtime
            .mark_snapshot_failed(&pane_id, &session_id, token)
            .unwrap(),
        "an old error cannot dirty a replacement request"
    );
    assert!(!runtime.pane(&pane_id).unwrap().snapshot_dirty);
    assert_eq!(
        runtime
            .pane(&pane_id)
            .unwrap()
            .snapshot_in_flight
            .as_ref()
            .unwrap()
            .token,
        rebound_token
    );
    assert!(
        runtime
            .mark_snapshot_failed(&pane_id, &session_id, rebound_token)
            .unwrap()
    );
    assert!(
        runtime
            .finish_snapshot_request(&pane_id, &session_id, rebound_token, false, true)
            .unwrap(),
        "a binding installed while the old request was in flight gets one coalesced retry"
    );
    assert!(runtime.snapshot_needs_refresh(&pane_id));
}

/// pane 被关闭并以同一 ID 重新创建后，旧 session 回执不能结束新 session 的在途请求。
///
/// @author kongweiguang
#[test]
fn stale_snapshot_completion_cannot_clear_rebound_session_request() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let persisted = pane(pane_id.clone());
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot(persisted.clone(), tab_id.clone()))
        .expect("valid v3 snapshot");
    let old_session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            old_session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .unwrap();
    let (_, old_token, _) = runtime
        .begin_snapshot(&pane_id)
        .unwrap()
        .expect("old session request starts");

    runtime.begin_close(&pane_id).unwrap();
    assert!(
        runtime
            .remove_closed_session(&pane_id, &old_session_id)
            .unwrap()
    );
    runtime
        .register_pending_pane(tab_id, persisted)
        .expect("closed pane identity can be restored");
    let new_session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            new_session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .unwrap();
    assert!(runtime.begin_snapshot(&pane_id).unwrap().is_some());

    assert!(
        !runtime
            .finish_snapshot_request(&pane_id, &old_session_id, old_token, false, true)
            .unwrap()
    );
    assert!(runtime.begin_snapshot(&pane_id).unwrap().is_none());
    assert_eq!(
        runtime.pane(&pane_id).unwrap().session_id.as_ref(),
        Some(&new_session_id)
    );
}

/// 只接受精确清理代次的关闭成功回执，避免测试绕过生产 owner 状态机直接删记录。
#[test]
fn orphan_cleanup_owner_is_queryable_and_removed_by_matching_generation() {
    let mut runtime = WorkspaceRuntime::default();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let size = TerminalSize::new(90, 28, 900, 560).unwrap();

    runtime
        .retain_orphan_cleanup(OrphanCleanupInput {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
            session_id: session_id.clone(),
            target: local_target(),
            size,
            snapshot: None,
            status: SharedString::from("cleanup required"),
            external_lease: None,
            transient: false,
        })
        .expect("orphan cleanup owner is retained");

    let owner = runtime
        .orphan_cleanup(&session_id)
        .expect("owner remains visible to cleanup UI");
    assert_eq!(owner.tab_id, tab_id);
    assert_eq!(owner.pane_id, pane_id);
    assert_eq!(owner.session_id, session_id);
    assert_eq!(owner.size, size);
    assert_eq!(runtime.orphan_cleanup_sessions().count(), 1);
    let generation = runtime
        .begin_orphan_cleanup(&session_id, SharedString::from("closing"))
        .expect("retained owner can enter cleanup");
    assert!(
        runtime
            .finish_orphan_cleanup(&SessionId::new(), generation)
            .is_none()
    );
    assert!(
        runtime
            .finish_orphan_cleanup(&session_id, generation + 1)
            .is_none()
    );
    assert!(
        runtime
            .orphan_cleanup(&session_id)
            .unwrap()
            .cleanup_in_flight
    );
    assert!(
        runtime
            .finish_orphan_cleanup(&session_id, generation)
            .is_some()
    );
    assert!(runtime.orphan_cleanup_sessions().next().is_none());
}

/// 新 tab 保存失败可把唯一 Ready owner 原子转交现有 orphan 重试流程，避免无 Tab session 泄漏。
///
/// @author kongweiguang
#[test]
fn ready_single_pane_owner_moves_atomically_to_orphan_cleanup() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let mut runtime =
        WorkspaceRuntime::from_snapshot(&snapshot(pane(pane_id.clone()), tab_id.clone()))
            .expect("valid single-pane workspace snapshot");
    let session_id = SessionId::new();
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .expect("ready session owner is installed");

    let moved_session = runtime
        .move_ready_pane_to_orphan_cleanup(
            &pane_id,
            SharedString::from("workspace persistence failed"),
        )
        .expect("failed new tab transfers its resource owner");

    assert_eq!(moved_session, session_id);
    assert!(runtime.pane(&pane_id).is_none());
    assert_eq!(runtime.pane_for_session(&session_id), None);
    assert!(runtime.tab_layout(&tab_id).is_none());
    let owner = runtime
        .orphan_cleanup(&session_id)
        .expect("same session remains owned by cleanup retry state");
    assert_eq!(owner.tab_id, tab_id);
    assert_eq!(owner.pane_id, pane_id);
    assert_eq!(owner.session_id, session_id);
    assert_eq!(owner.status.as_ref(), "workspace persistence failed");
    let generation = runtime
        .begin_orphan_cleanup(&session_id, SharedString::from("closing"))
        .expect("transferred owner can enter existing retry flow");
    assert!(
        runtime
            .finish_orphan_cleanup(&session_id, generation)
            .is_some()
    );
    assert!(runtime.orphan_cleanup(&session_id).is_none());
}

/// Runtime 只把全 transient 布局标成完整临时 tab，让持久化投影折叠混合分屏而保留正常 pane。
#[test]
fn mixed_transient_pane_projects_to_persisted_layout_before_cas_request() {
    let persistent_pane = pane(PaneId::new());
    let transient_pane = pane(PaneId::new());
    let tab_id = TabId::new();
    let mut source = snapshot(persistent_pane.clone(), tab_id.clone());
    source.terminal_panes.push(transient_pane.clone());
    let layout = TerminalLayoutNode::pane(persistent_pane.id.clone())
        .split_pane(
            &persistent_pane.id,
            transient_pane.id.clone(),
            SplitDirection::Horizontal,
            "split-test",
            false,
        )
        .expect("split keeps both pane identities");
    let Some(WorkspaceSessionTab::Terminal {
        layout: tab_layout, ..
    }) = source.terminal_tabs.first_mut()
    else {
        panic!("test snapshot has one terminal tab");
    };
    *tab_layout = layout;

    let mut runtime = WorkspaceRuntime::from_snapshot(&source).expect("valid mixed layout");
    runtime
        .set_resource_owner(&transient_pane.id, None, true)
        .expect("runtime tracks transient pane");
    let (transient_tabs, transient_panes) = runtime.transient_ids();
    assert!(
        transient_tabs.is_empty(),
        "mixed tab must not be dropped whole"
    );
    assert_eq!(transient_panes, [transient_pane.id.clone()].into());

    let mut controller = WorkspaceController::from_missing().expect("empty v3 baseline");
    controller
        .synchronize_snapshot(source.clone())
        .expect("complete runtime view is valid");
    let persisted = without_transient_resources(&source, &transient_tabs, &transient_panes)
        .expect("projection retains safe pane");
    let request = controller
        .begin_save_projected(persisted.clone())
        .expect("projected CAS can begin")
        .expect("persistent pane differs from empty baseline");

    assert_eq!(request.source_snapshot, source);
    assert_eq!(request.desired_snapshot, persisted);
    assert_eq!(request.desired_snapshot.terminal_panes.len(), 1);
    assert_eq!(
        request.desired_snapshot.terminal_panes[0].id,
        persistent_pane.id
    );
    assert_eq!(request.desired_snapshot.terminal_tabs.len(), 1);
    assert_eq!(
        request.desired_snapshot.terminal_tabs[0]
            .terminal_layout()
            .expect("terminal tab retains a layout")
            .pane_ids(),
        vec![persistent_pane.id]
    );
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Saving { .. }
    ));
}
