//! Root 工作区目标恢复与持久化边界测试。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::PathBuf,
};

use gpui_kit::SharedString;
use kerminal_application::{
    PublicErrorCode,
    workspace::{
        WorkspaceFileAccess, WorkspaceFileSource, WorkspaceLocalMachineScope,
        WorkspaceMachineStatus, WorkspacePaneMode, WorkspaceSessionDocument,
        WorkspaceSessionExpectedRevision, WorkspaceSessionInvalidReason, WorkspaceSessionLoad,
        WorkspaceSessionPane, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
        WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef, WorkspaceTmuxBinding,
    },
};
use tokio_util::sync::CancellationToken;

use kerminal_domain::{
    files::ContainerRuntime,
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{ContainerTarget, TmuxTarget},
    sessions::{
        ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget, SessionInfo,
        SessionLifecycle, SessionOwner, SessionTarget, SshInitialCommand, SshSessionTarget,
        TelnetSessionTarget, TerminalSize,
    },
    workspace::{SplitDirection, TerminalLayoutNode},
};

use crate::{
    features::{
        workspace_controller::{
            WorkspaceController, WorkspaceControllerEffect, WorkspacePersistenceState,
            WorkspaceResourceOperation, WorkspaceRestoreFailure, WorkspaceRestoreItemId,
            WorkspaceRestoreStatus,
        },
        workspace_panes::WorkspacePaneEvent,
        workspace_tabs::{WorkspaceTabEvent, WorkspaceTabKind},
    },
    root::{
        CenterPanel, PendingPaneFocusIntent, PendingTerminalCreationOwner,
        WorkspaceTerminalOpenKind,
        pane_input_runtime::advance_new_workspace_pane_focus_restore_generation,
        rollback_unpublished_terminal_creation_state,
        terminal_open::{
            can_discard_unpublished_transient_local_container_reservation,
            discard_unpublished_transient_local_container_reservation,
            failed_local_container_session_matches, unpublished_transient_local_container_target,
        },
        workspace_pane_for_terminal_target,
        workspace_runtime::{OrphanCleanupInput, RuntimePanePhase, WorkspaceRuntime},
        workspace_sessions::{
            StartupWorkspaceFocusGate, container_exec_command, initial_workspace_file_center_panel,
            record_workspace_restore_failure, startup_workspace_terminal_focus_target,
            tmux_binding_matches_target,
        },
        workspace_transient::without_transient_resources,
    },
};

/// 新终端 owner 必须完整匹配，旧恢复代次不得解除后续创建请求的忙状态。
///
/// @author kongweiguang
#[test]
fn pending_terminal_creation_owner_rejects_stale_generation() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let owner = PendingTerminalCreationOwner {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        generation: 7,
    };

    assert!(owner.matches(&tab_id, &pane_id, 7));
    assert!(!owner.matches(&tab_id, &pane_id, 6));
    assert!(!owner.matches(&tab_id, &PaneId::new(), 7));
    assert!(!owner.matches(&TabId::new(), &pane_id, 7));
}

/// 失败新建 reservation 只清理当前 owner；旧代次回执不能删除后续复用同身份的 pane。
///
/// @author kongweiguang
#[test]
fn unpublished_terminal_creation_rollback_removes_ghost_and_rejects_stale_generation() {
    let mut runtime = WorkspaceRuntime::default();
    let (pane, _) = workspace_pane_for_terminal_target(None, "local", None);
    let tab_id = TabId::new();
    let pane_id = pane.id.clone();
    runtime
        .register_pending_pane(tab_id.clone(), pane.clone())
        .expect("reservation pane is registered");
    runtime.register_tab_layout(tab_id.clone(), TerminalLayoutNode::pane(pane_id.clone()));
    runtime
        .mark_restore_failed(&pane_id)
        .expect("a failed empty reservation can be removed");

    let controller = WorkspaceController::from_missing().expect("empty controller");
    let mut generations = HashMap::from([(pane_id.clone(), 11)]);
    let cancellation = CancellationToken::new();
    let mut cancellations = HashMap::from([(pane_id.clone(), cancellation.clone())]);
    let mut owner = Some(PendingTerminalCreationOwner {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        generation: 11,
    });
    let mut is_creating = true;
    let mut pending_focus = Some(PendingPaneFocusIntent {
        generation: 1,
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        session_id: None,
        restore_generation: Some(11),
        expected_focus: None,
    });
    #[cfg(feature = "acceptance-metrics")]
    let mut probes = HashMap::from([(pane_id.clone(), 11)]);

    assert!(rollback_unpublished_terminal_creation_state(
        &mut runtime,
        Some(&controller),
        false,
        &mut generations,
        &mut cancellations,
        &mut owner,
        &mut is_creating,
        &mut pending_focus,
        #[cfg(feature = "acceptance-metrics")]
        &mut probes,
        &tab_id,
        &pane_id,
        11,
    ));
    assert!(runtime.pane(&pane_id).is_none());
    assert!(runtime.tab_layout(&tab_id).is_none());
    assert!(cancellation.is_cancelled());
    assert!(!cancellations.contains_key(&pane_id));
    assert!(!generations.contains_key(&pane_id));
    assert!(owner.is_none());
    assert!(!is_creating);
    assert!(pending_focus.is_none());
    #[cfg(feature = "acceptance-metrics")]
    assert!(!probes.contains_key(&pane_id));

    runtime
        .register_pending_pane(tab_id.clone(), pane)
        .expect("the same stable identity can model a newer restore generation");
    runtime.register_tab_layout(tab_id.clone(), TerminalLayoutNode::pane(pane_id.clone()));
    runtime
        .mark_restore_failed(&pane_id)
        .expect("the newer reservation is failed independently");
    generations.insert(pane_id.clone(), 12);
    let newer_cancellation = CancellationToken::new();
    cancellations.insert(pane_id.clone(), newer_cancellation.clone());
    owner = Some(PendingTerminalCreationOwner {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        generation: 12,
    });
    is_creating = true;
    pending_focus = Some(PendingPaneFocusIntent {
        generation: 2,
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        session_id: None,
        restore_generation: Some(12),
        expected_focus: None,
    });

    assert!(!rollback_unpublished_terminal_creation_state(
        &mut runtime,
        Some(&controller),
        false,
        &mut generations,
        &mut cancellations,
        &mut owner,
        &mut is_creating,
        &mut pending_focus,
        #[cfg(feature = "acceptance-metrics")]
        &mut probes,
        &tab_id,
        &pane_id,
        11,
    ));
    assert!(runtime.pane(&pane_id).is_some());
    assert!(runtime.tab_layout(&tab_id).is_some());
    assert!(!newer_cancellation.is_cancelled());
    assert_eq!(generations.get(&pane_id), Some(&12));
    assert_eq!(owner.as_ref().map(|owner| owner.generation), Some(12));
    assert!(is_creating);
    assert!(pending_focus.is_some());
}

/// 回滚拒绝已发布 tab、分屏布局、session 与 orphan cleanup owner，避免把真实工作区资源误删。
///
/// @author kongweiguang
#[test]
fn unpublished_terminal_rollback_preserves_published_split_and_cleanup_resources() {
    let (pane, _) = workspace_pane_for_terminal_target(None, "local", None);
    let pane_id = pane.id.clone();
    let (tab_id, snapshot) = workspace_snapshot_from_pane(pane.clone());
    let mut published_controller = WorkspaceController::from_missing().expect("empty controller");
    published_controller
        .synchronize_snapshot(snapshot)
        .expect("fixture is a valid published terminal tab");
    let mut published_runtime = WorkspaceRuntime::default();
    published_runtime
        .register_pending_pane(tab_id.clone(), pane.clone())
        .expect("published pane runtime can be constructed");
    published_runtime
        .register_tab_layout(tab_id.clone(), TerminalLayoutNode::pane(pane_id.clone()));
    published_runtime
        .mark_restore_failed(&pane_id)
        .expect("published pane is failed for retry");
    let mut generations = HashMap::from([(pane_id.clone(), 3)]);
    let token = CancellationToken::new();
    let mut cancellations = HashMap::from([(pane_id.clone(), token.clone())]);
    let mut owner = Some(PendingTerminalCreationOwner {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        generation: 3,
    });
    let mut is_creating = true;
    let mut pending_focus = None;
    #[cfg(feature = "acceptance-metrics")]
    let mut probes = HashMap::new();

    assert!(!rollback_unpublished_terminal_creation_state(
        &mut published_runtime,
        Some(&published_controller),
        false,
        &mut generations,
        &mut cancellations,
        &mut owner,
        &mut is_creating,
        &mut pending_focus,
        #[cfg(feature = "acceptance-metrics")]
        &mut probes,
        &tab_id,
        &pane_id,
        3,
    ));
    assert!(published_runtime.pane(&pane_id).is_some());
    assert!(published_runtime.tab_layout(&tab_id).is_some());
    assert!(!token.is_cancelled());

    let mut split_runtime = WorkspaceRuntime::default();
    let (split_pane, _) = workspace_pane_for_terminal_target(None, "split", None);
    let split_pane_id = split_pane.id.clone();
    let (sibling_pane, _) = workspace_pane_for_terminal_target(None, "sibling", None);
    let sibling_pane_id = sibling_pane.id.clone();
    let split_tab_id = TabId::new();
    split_runtime
        .register_pending_pane(split_tab_id.clone(), split_pane)
        .expect("split candidate pane is registered");
    split_runtime
        .register_pending_pane(split_tab_id.clone(), sibling_pane)
        .expect("sibling pane is registered");
    split_runtime.register_tab_layout(
        split_tab_id.clone(),
        TerminalLayoutNode::Split {
            id: "failed-split".to_owned(),
            direction: SplitDirection::Horizontal,
            children: vec![
                TerminalLayoutNode::pane(split_pane_id.clone()),
                TerminalLayoutNode::pane(sibling_pane_id),
            ],
            sizes: None,
        },
    );
    split_runtime
        .mark_restore_failed(&split_pane_id)
        .expect("split candidate failure is retained for the split owner");
    let split_controller = WorkspaceController::from_missing().expect("empty controller");
    let mut split_generations = HashMap::from([(split_pane_id.clone(), 4)]);
    let split_token = CancellationToken::new();
    let mut split_cancellations = HashMap::from([(split_pane_id.clone(), split_token.clone())]);
    let mut split_owner = Some(PendingTerminalCreationOwner {
        tab_id: split_tab_id.clone(),
        pane_id: split_pane_id.clone(),
        generation: 4,
    });
    let mut split_is_creating = true;
    let mut split_focus = None;
    #[cfg(feature = "acceptance-metrics")]
    let mut split_probes = HashMap::new();

    assert!(!rollback_unpublished_terminal_creation_state(
        &mut split_runtime,
        Some(&split_controller),
        false,
        &mut split_generations,
        &mut split_cancellations,
        &mut split_owner,
        &mut split_is_creating,
        &mut split_focus,
        #[cfg(feature = "acceptance-metrics")]
        &mut split_probes,
        &split_tab_id,
        &split_pane_id,
        4,
    ));
    assert!(split_runtime.pane(&split_pane_id).is_some());
    assert!(split_runtime.tab_layout(&split_tab_id).is_some());
    assert!(!split_token.is_cancelled());

    let mut cleanup_runtime = WorkspaceRuntime::default();
    let (cleanup_pane, _) = workspace_pane_for_terminal_target(None, "cleanup", None);
    let cleanup_pane_id = cleanup_pane.id.clone();
    let cleanup_tab_id = TabId::new();
    cleanup_runtime
        .register_pending_pane(cleanup_tab_id.clone(), cleanup_pane)
        .expect("cleanup-owned pane is registered");
    cleanup_runtime.register_tab_layout(
        cleanup_tab_id.clone(),
        TerminalLayoutNode::pane(cleanup_pane_id.clone()),
    );
    let cleanup_session_id = SessionId::new();
    let cleanup_target = SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    });
    cleanup_runtime
        .retain_cleanup_session(
            &cleanup_pane_id,
            cleanup_session_id.clone(),
            cleanup_target,
            TerminalSize::default(),
            None,
            SharedString::from("cleanup pending"),
        )
        .expect("cleanup session remains bound to its pane");
    let cleanup_controller = WorkspaceController::from_missing().expect("empty controller");
    let mut cleanup_generations = HashMap::from([(cleanup_pane_id.clone(), 5)]);
    let cleanup_token = CancellationToken::new();
    let mut cleanup_cancellations =
        HashMap::from([(cleanup_pane_id.clone(), cleanup_token.clone())]);
    let mut cleanup_owner = Some(PendingTerminalCreationOwner {
        tab_id: cleanup_tab_id.clone(),
        pane_id: cleanup_pane_id.clone(),
        generation: 5,
    });
    let mut cleanup_is_creating = true;
    let mut cleanup_focus = None;
    #[cfg(feature = "acceptance-metrics")]
    let mut cleanup_probes = HashMap::new();

    assert!(!rollback_unpublished_terminal_creation_state(
        &mut cleanup_runtime,
        Some(&cleanup_controller),
        false,
        &mut cleanup_generations,
        &mut cleanup_cancellations,
        &mut cleanup_owner,
        &mut cleanup_is_creating,
        &mut cleanup_focus,
        #[cfg(feature = "acceptance-metrics")]
        &mut cleanup_probes,
        &cleanup_tab_id,
        &cleanup_pane_id,
        5,
    ));
    assert_eq!(
        cleanup_runtime.session_for_pane(&cleanup_pane_id),
        Some(&cleanup_session_id)
    );
    assert!(cleanup_runtime.tab_layout(&cleanup_tab_id).is_some());
    assert!(!cleanup_token.is_cancelled());
}

/// 恢复活动文件标签时选中其中央工作区，并尊重既有启动用户导航代次。
///
/// @author kongweiguang
#[test]
fn startup_workspace_file_tab_respects_existing_navigation_generation() {
    let transfer = WorkspaceSessionTab::SftpTransfer {
        id: TabId::new(),
        tab_group_id: None,
        title: "Transfer".to_owned(),
        machine_id: "legacy-machine".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: Some("/srv".to_owned()),
        initial_right_selection: None,
    };
    let file = WorkspaceSessionTab::WorkspaceFile {
        id: TabId::new(),
        tab_group_id: None,
        title: "notes.txt".to_owned(),
        machine_id: "local".to_owned(),
        target: WorkspaceTargetRef::Local { profile_id: None },
        path: "C:/workspace/notes.txt".to_owned(),
        root_path: None,
        access: WorkspaceFileAccess::Editable,
        source: WorkspaceFileSource::Local,
    };

    assert_eq!(
        initial_workspace_file_center_panel(true, 0, CenterPanel::Terminal, Some(&transfer),),
        Some(CenterPanel::Files),
        "a persisted active SFTP tab must render its restored FilesPane",
    );
    assert_eq!(
        initial_workspace_file_center_panel(true, 0, CenterPanel::Terminal, Some(&file)),
        Some(CenterPanel::Editor),
        "a persisted active workspace-file tab must render its editor",
    );
    assert_eq!(
        initial_workspace_file_center_panel(true, 0, CenterPanel::Commands, Some(&transfer)),
        None,
        "startup loading must not steal a different selected center view",
    );
    assert_eq!(
        initial_workspace_file_center_panel(true, 1, CenterPanel::Terminal, Some(&transfer)),
        None,
        "an explicit user navigation back to Terminal must not be overridden",
    );
    assert_eq!(
        initial_workspace_file_center_panel(false, 0, CenterPanel::Terminal, Some(&transfer)),
        None,
        "later snapshot installation must not change the center view",
    );
}

/// 成功读取不长期占用工作区高度，分类异常和读取错误仍明确提示且保留原数据边界。
///
/// @author kongweiguang
#[test]
fn workspace_session_notice_silences_only_missing_or_valid_loaded_state() {
    let loaded = WorkspaceSessionLoad::Loaded {
        document: WorkspaceSessionDocument::empty_v3(),
        revision: WorkspaceSessionRevision::new("notice-test-revision"),
    };
    let missing = WorkspaceSessionLoad::Missing;
    let invalid = WorkspaceSessionLoad::Invalid {
        reason: WorkspaceSessionInvalidReason::MalformedJson,
    };
    let unsupported = WorkspaceSessionLoad::Unsupported { version: 4 };

    assert!(crate::root::workspace_session_notice(None, None).is_none());
    assert!(crate::root::workspace_session_notice(Some(&missing), None).is_none());
    assert!(crate::root::workspace_session_notice(Some(&loaded), None).is_none());

    let invalid_notice = crate::root::workspace_session_notice(Some(&invalid), None)
        .expect("损坏工作区必须保留可见恢复提示")
        .to_string();
    assert!(invalid_notice.contains("JSON 格式错误"));
    assert!(invalid_notice.contains("原数据未修改"));

    let unsupported_notice = crate::root::workspace_session_notice(Some(&unsupported), None)
        .expect("未来版本必须保留可见恢复提示")
        .to_string();
    assert!(unsupported_notice.contains("不支持工作区版本 v4"));
    assert!(unsupported_notice.contains("原数据未修改"));

    let read_error = crate::root::workspace_session_notice(Some(&loaded), Some("读取失败样本"))
        .expect("读取错误必须优先于成功快照显示")
        .to_string();
    assert!(read_error.contains("工作区读取失败"));
    assert!(read_error.contains("原数据未修改"));
    assert!(read_error.contains("读取失败样本"));
}

/// 新建 Profile 的预留与解析虽使用不同恢复代次，但原用户焦点与导航代次必须保持不变。
///
/// @author kongweiguang
#[test]
fn new_profile_focus_intent_follows_only_its_reserved_restore_generation() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut intent = PendingPaneFocusIntent {
        generation: 7,
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        session_id: None,
        restore_generation: Some(1),
        expected_focus: None,
    };

    assert!(advance_new_workspace_pane_focus_restore_generation(
        Some(&mut intent),
        &tab_id,
        &pane_id,
        1,
        2,
    ));
    assert_eq!(intent.restore_generation, Some(2));
    assert_eq!(intent.generation, 7, "不能把旧导航意图变成一次新聚焦");
    assert_eq!(intent.tab_id, tab_id);
    assert_eq!(intent.pane_id, pane_id);
    assert_eq!(intent.session_id, None);
    assert_eq!(intent.expected_focus, None);

    assert!(!advance_new_workspace_pane_focus_restore_generation(
        Some(&mut intent),
        &tab_id,
        &pane_id,
        1,
        3,
    ));
    assert_eq!(
        intent.restore_generation,
        Some(2),
        "迟到旧交接不能改写新代次"
    );
}

/// 用户已导航、已取消、目标已换或 session 已绑定时，解析不能注入或复活初始焦点。
///
/// @author kongweiguang
#[test]
fn new_profile_focus_handoff_rejects_missing_changed_or_bound_intent() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    assert!(!advance_new_workspace_pane_focus_restore_generation(
        None, &tab_id, &pane_id, 1, 2,
    ));

    let mut intent = PendingPaneFocusIntent {
        generation: 7,
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
        session_id: None,
        restore_generation: Some(1),
        expected_focus: None,
    };
    for (request_tab, request_pane, previous_generation) in [
        (TabId::new(), pane_id.clone(), 1),
        (tab_id.clone(), PaneId::new(), 1),
        (tab_id.clone(), pane_id.clone(), 9),
    ] {
        assert!(!advance_new_workspace_pane_focus_restore_generation(
            Some(&mut intent),
            &request_tab,
            &request_pane,
            previous_generation,
            2,
        ));
        assert_eq!(intent.restore_generation, Some(1));
    }
    intent.session_id = Some(SessionId::new());
    assert!(!advance_new_workspace_pane_focus_restore_generation(
        Some(&mut intent),
        &tab_id,
        &pane_id,
        1,
        2,
    ));
    assert_eq!(intent.restore_generation, Some(1));
}

/// 启动自动焦点仅覆盖首次、前台、Terminal 中仍由启动焦点拥有的活动 terminal pane。
///
/// @author kongweiguang
#[test]
fn startup_terminal_focus_gate_rejects_late_or_user_owned_focus() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let layout = TerminalLayoutNode::pane(pane_id.clone());
    let gate = StartupWorkspaceFocusGate {
        initial_install: true,
        window_active: true,
        terminal_center: true,
        startup_focus_owned: true,
        modal_open: false,
        focus_intent_pending: false,
    };

    assert_eq!(
        startup_workspace_terminal_focus_target(gate, Some((&tab_id, &layout)), Some(&pane_id)),
        Some((tab_id.clone(), pane_id.clone()))
    );
    assert_eq!(
        startup_workspace_terminal_focus_target(gate, Some((&tab_id, &layout)), None),
        Some((tab_id.clone(), pane_id.clone())),
        "缺失焦点按现有活动标签规则落到首 pane，而非猜其它会话"
    );
    assert!(
        startup_workspace_terminal_focus_target(gate, None, None).is_none(),
        "非 Terminal 活动标签不会推导终端焦点"
    );

    let denied_gates = [
        StartupWorkspaceFocusGate {
            initial_install: false,
            ..gate
        },
        StartupWorkspaceFocusGate {
            window_active: false,
            ..gate
        },
        StartupWorkspaceFocusGate {
            terminal_center: false,
            ..gate
        },
        StartupWorkspaceFocusGate {
            startup_focus_owned: false,
            ..gate
        },
        StartupWorkspaceFocusGate {
            modal_open: true,
            ..gate
        },
        StartupWorkspaceFocusGate {
            focus_intent_pending: true,
            ..gate
        },
    ];
    for denied in denied_gates {
        assert!(
            startup_workspace_terminal_focus_target(
                denied,
                Some((&tab_id, &layout)),
                Some(&pane_id)
            )
            .is_none(),
            "每个否决条件都必须阻止首次恢复焦点"
        );
    }
}

/// 停止期间的取消回执仍更新真实 controller/runtime，保留原目标且不新增 live session 或修改持久快照。
///
/// @author kongweiguang
#[test]
fn shutdown_cancelled_restore_records_resource_accounting_and_retains_target() {
    let pane = tmux_pane("shutdown-fixture");
    let pane_id = pane.id.clone();
    let (_, snapshot) = workspace_snapshot_from_pane(pane);
    let mut controller = WorkspaceController::from_missing().expect("empty controller");
    controller
        .synchronize_snapshot(snapshot.clone())
        .expect("install fixture");
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot).expect("pending runtime");
    let target = SessionTarget::Local(LocalSessionTarget {
        program: "fixture-shell".into(),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    });
    let message = record_workspace_restore_failure(
        &mut runtime,
        Some(&mut controller),
        &pane_id,
        WorkspaceRestoreFailure::Cancelled,
        Some(target.clone()),
        "固定取消回执".into(),
    );
    assert_eq!(message.as_ref(), "固定取消回执");
    let pane = runtime.pane(&pane_id).expect("owner entry is retained");
    assert_eq!(pane.phase, RuntimePanePhase::RestoreFailed);
    assert_eq!(pane.target.as_ref(), Some(&target));
    assert!(
        pane.session_id.is_none(),
        "resolve receipt must not create a PTY"
    );
    assert_eq!(
        controller.restore_status(&WorkspaceRestoreItemId::Pane(pane_id)),
        Some(WorkspaceRestoreStatus::Failed(
            WorkspaceRestoreFailure::Cancelled
        ))
    );
    assert_eq!(controller.snapshot(), &snapshot);
}

/// 真实失败类型不能因宿主收尾而被压成成功或通用取消，controller 必须保留可核对的原始分类。
///
/// @author kongweiguang
#[test]
fn shutdown_restore_failure_keeps_its_actual_failure_class() {
    let pane = tmux_pane("shutdown-failure-fixture");
    let pane_id = pane.id.clone();
    let (_, snapshot) = workspace_snapshot_from_pane(pane);
    let mut controller = WorkspaceController::from_missing().expect("empty controller");
    controller
        .synchronize_snapshot(snapshot.clone())
        .expect("install fixture");
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot).expect("pending runtime");
    let message = record_workspace_restore_failure(
        &mut runtime,
        Some(&mut controller),
        &pane_id,
        WorkspaceRestoreFailure::ResourceFailure,
        None,
        "固定真实失败".into(),
    );
    assert_eq!(message.as_ref(), "固定真实失败");
    assert_eq!(
        controller.restore_status(&WorkspaceRestoreItemId::Pane(pane_id)),
        Some(WorkspaceRestoreStatus::Failed(
            WorkspaceRestoreFailure::ResourceFailure
        ))
    );
    assert_eq!(controller.snapshot(), &snapshot);
}

/// 重命名标签后分屏必须提交原用户标题，并用同一 CAS 基线保存已确认的新布局。
///
/// @author kongweiguang
#[test]
fn renamed_tab_split_ack_saves_committed_layout_and_rejects_conflicts() {
    let pane = tmux_pane("profile-fixture");
    let source_pane_id = pane.id.clone();
    let (tab_id, initial_snapshot) = workspace_snapshot_from_pane(pane);
    let mut controller = WorkspaceController::from_missing().expect("empty v3 baseline");
    controller
        .synchronize_snapshot(initial_snapshot.clone())
        .expect("initial pane enters the controller");

    let initial_save = controller
        .begin_save_projected(initial_snapshot)
        .expect("initial projected snapshot is valid")
        .expect("initial workspace needs a CAS write");
    controller
        .finish_save(
            &initial_save.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("split-baseline"),
            },
        )
        .expect("initial save acknowledgement establishes the CAS baseline");

    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::RenameTab {
                tab_id: tab_id.clone(),
                title: "想".to_owned(),
            })
            .expect("renaming the existing tab is a local snapshot change"),
        WorkspaceControllerEffect::Changed { .. }
    ));
    let renamed_snapshot = controller.snapshot().clone();
    let (transient_tabs, transient_panes) = WorkspaceRuntime::default().transient_ids();
    let renamed_persisted =
        without_transient_resources(&renamed_snapshot, &transient_tabs, &transient_panes)
            .expect("renamed tab remains in the normal persistence projection");
    let rename_save = controller
        .begin_save_projected(renamed_persisted)
        .expect("the renamed snapshot is valid for CAS")
        .expect("renaming changes the persisted snapshot");

    // 真实重复 pane ID 仍须拒绝，且不能替换已接受的快照。
    let mut conflicting_snapshot = renamed_snapshot.clone();
    conflicting_snapshot
        .terminal_panes
        .push(conflicting_snapshot.terminal_panes[0].clone());
    assert!(
        controller
            .synchronize_snapshot(conflicting_snapshot)
            .is_err()
    );
    assert_eq!(controller.snapshot(), &renamed_snapshot);

    let split = controller
        .apply_pane_event(
            &tab_id,
            WorkspacePaneEvent::Split {
                pane_id: source_pane_id.clone(),
                direction: SplitDirection::Horizontal,
                before: false,
            },
        )
        .expect("splitting a supported pane creates a pending resource operation");
    let WorkspaceControllerEffect::ResourceOperation(WorkspaceResourceOperation::CreatePane {
        operation_id,
        pane: created_pane,
        ..
    }) = split
    else {
        panic!("split must wait for its real pane resource acknowledgement");
    };
    let created_pane_id = created_pane.id.clone();
    assert_ne!(created_pane_id, source_pane_id);

    // 迟到的资源回执不能提交候选布局，也不能清除当前 pending 操作。
    assert!(
        controller
            .complete_resource_operation("stale-split-ack", Ok(()))
            .is_err()
    );
    assert_eq!(controller.snapshot(), &renamed_snapshot);
    assert_eq!(
        controller
            .pending_resource_operation()
            .map(WorkspaceResourceOperation::operation_id),
        Some(operation_id.as_str())
    );

    assert!(matches!(
        controller
            .complete_resource_operation(&operation_id, Ok(()))
            .expect("only the exact resource acknowledgement commits the split"),
        WorkspaceControllerEffect::Changed { .. }
    ));
    let committed = controller.snapshot();
    let terminal_tab = committed
        .terminal_tabs
        .iter()
        .find(|tab| tab.id() == &tab_id)
        .expect("the renamed tab remains in the committed workspace");
    assert_eq!(terminal_tab.title(), "想");
    let layout = terminal_tab
        .terminal_layout()
        .expect("the renamed tab retains its terminal layout");
    let committed_pane_ids = layout.pane_ids();
    assert_eq!(committed_pane_ids.len(), 2);
    assert!(committed_pane_ids.contains(&source_pane_id));
    assert!(committed_pane_ids.contains(&created_pane_id));
    assert_eq!(committed.terminal_panes.len(), 2);

    // Split ACK 到达时旧的重命名 CAS 仍在途；Root 的立即保存请求应保留待重试状态。
    let persisted = without_transient_resources(committed, &transient_tabs, &transient_panes)
        .expect("the committed split has a valid persistence projection");
    assert!(
        controller
            .begin_save_projected(persisted.clone())
            .expect("an in-flight CAS leaves the split snapshot pending")
            .is_none()
    );

    controller
        .finish_save(
            &rename_save.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("split-after-rename"),
            },
        )
        .expect("the older rename CAS acknowledgement is accepted by request ID");
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Dirty
    ));

    let split_save = controller
        .begin_save_projected(persisted)
        .expect("the split snapshot is valid for the ordinary CAS path")
        .expect("the committed split differs from the persisted baseline");
    assert_eq!(
        split_save.expected_revision,
        WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(
            "split-after-rename"
        )),
        "the split save uses the revision returned by the rename CAS"
    );

    // 在应用保存 ACK 前，通过正式 workspace document codec 读回 CAS 候选。
    let readback = split_save
        .candidate_document
        .workspace_snapshot()
        .expect("the CAS candidate decodes through the production workspace codec");
    let readback_tab = readback
        .terminal_tabs
        .iter()
        .find(|tab| tab.id() == &tab_id)
        .expect("codec readback preserves the renamed tab");
    assert_eq!(readback_tab.title(), "想");
    assert_eq!(readback_tab.terminal_layout().unwrap().pane_ids().len(), 2);
    assert_eq!(readback.terminal_panes.len(), 2);

    controller
        .finish_save(
            &split_save.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("split-saved"),
            },
        )
        .expect("the exact CAS save acknowledgement is accepted");
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Clean
    ));
}

/// 同一已恢复 tab 的 A/B SSH pane 各用自身目标，不能因共享 tab 元数据拒绝 B pane。
///
/// @author kongweiguang
#[test]
fn mixed_ssh_split_ready_sync_matches_pane_identity_and_preserves_tab_metadata() {
    let tab_id = TabId::new();
    let pane_a_id = PaneId::new();
    let pane_b_id = PaneId::new();
    let host_a = HostId::parse("native:ssh-a").expect("valid host A identity");
    let host_b = HostId::parse("native:ssh-b").expect("valid host B identity");
    let pane_for_host = |id: PaneId, host_id: HostId, title: &str| WorkspaceSessionPane {
        id,
        title: title.to_owned(),
        machine_id: host_id.as_str().to_owned(),
        mode: WorkspacePaneMode::Ssh,
        target: Some(WorkspaceTargetRef::Ssh {
            host_id: host_id.clone(),
        }),
        remote_host_id: Some(host_id),
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
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: None,
        output_history: None,
    };
    let pane_a = pane_for_host(pane_a_id.clone(), host_a.clone(), "SSH A pane");
    let pane_b = pane_for_host(pane_b_id.clone(), host_b.clone(), "SSH B pane");
    let layout = TerminalLayoutNode::Split {
        id: "mixed-ssh-split".to_owned(),
        direction: SplitDirection::Horizontal,
        children: vec![
            TerminalLayoutNode::pane(pane_a_id.clone()),
            TerminalLayoutNode::pane(pane_b_id.clone()),
        ],
        sizes: None,
    };
    let tab = WorkspaceSessionTab::Terminal {
        id: tab_id.clone(),
        tab_group_id: None,
        title: "Shared SSH A/B tab".to_owned(),
        machine_id: host_a.as_str().to_owned(),
        layout: layout.clone(),
    };
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_b_id.clone()),
        selected_machine_id: Some(host_b.as_str().to_owned()),
        terminal_tabs: vec![tab.clone()],
        terminal_panes: vec![pane_a.clone(), pane_b.clone()],
        tab_groups: BTreeMap::new(),
    };
    snapshot
        .validate()
        .expect("mixed-host split preserves the valid v3 layout contract");

    let tab_metadata_before = match &snapshot.terminal_tabs[0] {
        WorkspaceSessionTab::Terminal {
            title, machine_id, ..
        } => (title.clone(), machine_id.clone()),
        _ => unreachable!("the mixed pane fixture uses a terminal tab"),
    };
    assert!(super::ready_workspace_pane_matches_snapshot(
        &tab_id, &pane_a_id, &pane_a, &tab, &pane_a, &layout,
    ));
    assert!(super::ready_workspace_pane_matches_snapshot(
        &tab_id, &pane_b_id, &pane_b, &tab, &pane_b, &layout,
    ));

    assert!(
        !super::ready_workspace_pane_matches_snapshot(
            &tab_id, &pane_a_id, &pane_b, &tab, &pane_b, &layout,
        ),
        "a pane row cannot be synchronized under another pane ID",
    );
    let mut wrong_target = pane_b.clone();
    wrong_target.target = Some(WorkspaceTargetRef::Ssh {
        host_id: host_a.clone(),
    });
    wrong_target.remote_host_id = Some(host_a);
    assert!(
        !super::ready_workspace_pane_matches_snapshot(
            &tab_id,
            &pane_b_id,
            &pane_b,
            &tab,
            &wrong_target,
            &layout,
        ),
        "a pane with a different typed SSH target must remain a conflict",
    );
    let layout_without_b = TerminalLayoutNode::pane(pane_a_id);
    assert!(
        !super::ready_workspace_pane_matches_snapshot(
            &tab_id,
            &pane_b_id,
            &pane_b,
            &tab,
            &pane_b,
            &layout_without_b,
        ),
        "the exact tab layout must still contain the pane being synchronized",
    );

    match &snapshot.terminal_tabs[0] {
        WorkspaceSessionTab::Terminal {
            title, machine_id, ..
        } => assert_eq!(
            (title.clone(), machine_id.clone()),
            tab_metadata_before,
            "ready matching must leave shared tab title and machine metadata untouched",
        ),
        _ => unreachable!("the mixed pane fixture uses a terminal tab"),
    }
}

/// 为 SSH 投影测试构造显式目标，分别验证可保存 shell 文本与仅留运行态的 argv。
fn ssh_target(command: Option<SshInitialCommand>) -> SessionTarget {
    SessionTarget::Ssh(SshSessionTarget {
        host_id: HostId::new(),
        working_directory: None,
        command,
    })
}

/// 将生产 projector 生成的 pane 放入可供 controller 和 CAS codec 使用的 v3 快照。
fn workspace_snapshot_from_pane(pane: WorkspaceSessionPane) -> (TabId, WorkspaceSessionSnapshot) {
    let tab_id = TabId::new();
    let layout = TerminalLayoutNode::pane(pane.id.clone());
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane.id.clone()),
        selected_machine_id: Some(pane.machine_id.clone()),
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: pane.title.clone(),
            machine_id: pane.machine_id.clone(),
            layout,
        }],
        terminal_panes: vec![pane],
        tab_groups: BTreeMap::new(),
    };
    (tab_id, snapshot)
}

/// 测试样本只保留容器命令编码所需字段，其他持久字段使用安全空值。
/// TelnetSessionTarget 不含 HostId；投影必须保留来源主机，才能让新 pane 通过工作区校验。
///
/// @author kongweiguang
#[test]
fn telnet_pane_projection_keeps_saved_host_identity() {
    let host_id = HostId::new();
    let target = SessionTarget::Telnet(TelnetSessionTarget {
        host: "127.0.0.1".to_owned(),
        port: 23,
    });
    let (pane, transient) =
        workspace_pane_for_terminal_target(Some(&target), "Telnet", Some(&host_id));

    assert!(
        !transient,
        "saved Telnet hosts have a stable workspace reference"
    );
    assert_eq!(pane.mode, WorkspacePaneMode::Telnet);
    assert_eq!(pane.machine_id, host_id.as_str());
    assert_eq!(pane.remote_host_id.as_ref(), Some(&host_id));
    assert_eq!(
        pane.target,
        Some(WorkspaceTargetRef::Telnet {
            host_id: host_id.clone(),
        })
    );

    let (_, snapshot) = workspace_snapshot_from_pane(pane);
    WorkspaceController::from_missing()
        .expect("missing workspace baseline")
        .synchronize_snapshot(snapshot)
        .expect("the newly created Telnet pane must pass the same target gate as SSH");
}

/// SerialSessionTarget 不含 HostId；显式来源身份使 pane 可恢复并被 controller 接受。
///
/// @author kongweiguang
#[test]
fn serial_pane_projection_keeps_saved_host_identity() {
    let host_id = HostId::new();
    let target = SessionTarget::Serial(SerialSessionTarget::new(PathBuf::from("COM7")));
    let (pane, transient) =
        workspace_pane_for_terminal_target(Some(&target), "Serial", Some(&host_id));

    assert!(
        !transient,
        "saved serial hosts have a stable workspace reference"
    );
    assert_eq!(pane.mode, WorkspacePaneMode::Serial);
    assert_eq!(pane.machine_id, host_id.as_str());
    assert_eq!(pane.remote_host_id.as_ref(), Some(&host_id));
    assert_eq!(
        pane.target,
        Some(WorkspaceTargetRef::Serial {
            host_id: host_id.clone(),
        })
    );

    let (_, snapshot) = workspace_snapshot_from_pane(pane);
    WorkspaceController::from_missing()
        .expect("missing workspace baseline")
        .synchronize_snapshot(snapshot)
        .expect("the newly created serial pane must pass the same target gate as SSH");
}

/// 测试样本只保留容器命令编码所需字段，其他持久字段使用安全空值。
fn container_pane(shell: &str, args: Vec<String>) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id: PaneId::new(),
        title: "test container".to_owned(),
        machine_id: "container-test".to_owned(),
        mode: WorkspacePaneMode::Container,
        target: None,
        remote_host_id: None,
        container_id: None,
        profile_id: None,
        shell: Some(shell.to_owned()),
        args: Some(args),
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: None,
        output_history: None,
    }
}

/// 构造携带显式本地 profile 的 tmux pane，供结构化身份匹配测试复用。
fn tmux_pane(profile_id: &str) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id: PaneId::new(),
        title: "tmux".to_owned(),
        machine_id: "machine-local".to_owned(),
        mode: WorkspacePaneMode::Local,
        target: Some(WorkspaceTargetRef::Local {
            profile_id: Some(profile_id.to_owned()),
        }),
        remote_host_id: None,
        container_id: None,
        profile_id: Some(profile_id.to_owned()),
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
        output_history: None,
    }
}

/// 确认真实 pane projector 到 CAS 文档保留原样 shell 文本，不把它改写成 argv。
#[test]
fn ssh_shell_command_round_trips_without_rewriting() {
    let command = "cd '/srv/app' && ./run";
    let target = ssh_target(Some(SshInitialCommand::Shell(command.to_owned())));
    let (pane, transient) = workspace_pane_for_terminal_target(Some(&target), "SSH", None);
    assert!(!transient, "可逆的 SSH shell 文本由生产 projector 保留");
    assert_eq!(pane.remote_command.as_deref(), Some(command));

    let (_tab_id, snapshot) = workspace_snapshot_from_pane(pane);
    let runtime = WorkspaceRuntime::from_snapshot(&snapshot).expect("projected v3 snapshot");
    let mut controller = WorkspaceController::from_missing().expect("empty v3 baseline");
    controller
        .synchronize_snapshot(snapshot.clone())
        .expect("live workspace accepts the projected pane");
    let (transient_tabs, transient_panes) = runtime.transient_ids();
    let persisted = without_transient_resources(&snapshot, &transient_tabs, &transient_panes)
        .expect("production persistence projection accepts SSH shell text");
    let request = controller
        .begin_save_projected(persisted)
        .expect("the normal CAS entry validates the save candidate")
        .expect("persistent shell target requires a save");
    let candidate = request
        .candidate_document
        .workspace_snapshot()
        .expect("candidate document decodes through the production codec");

    assert_eq!(candidate.terminal_panes.len(), 1);
    assert_eq!(
        candidate.terminal_panes[0].remote_command.as_deref(),
        Some(command)
    );
    assert_eq!(
        request.candidate_document.as_value()["terminalPanes"][0]["remoteCommand"].as_str(),
        Some(command),
        "CAS candidate keeps the exact on-disk shell text"
    );
}

/// 确认结构化 SSH argv 仅留在 transient 运行态，真实 CAS 候选不会包含可重放命令。
#[test]
fn ssh_process_argv_is_transient_and_not_written_to_workspace_v3() {
    let target = ssh_target(Some(SshInitialCommand::Process {
        program: "tool".to_owned(),
        args: vec!["arg with spaces".to_owned()],
    }));
    let (process_pane, transient) = workspace_pane_for_terminal_target(Some(&target), "SSH", None);
    assert!(
        transient,
        "生产 projector 必须将一次性 argv 标记为 transient"
    );
    assert_eq!(process_pane.remote_command, None);
    let process_pane_id = process_pane.id.clone();
    let process_target = target.clone();
    let (process_tab_id, mut snapshot) = workspace_snapshot_from_pane(process_pane);

    let durable_target = ssh_target(None);
    let (durable_pane, durable_transient) =
        workspace_pane_for_terminal_target(Some(&durable_target), "Saved SSH", None);
    assert!(!durable_transient);
    let durable_pane_id = durable_pane.id.clone();
    let durable_tab_id = TabId::new();
    snapshot.terminal_tabs.push(WorkspaceSessionTab::Terminal {
        id: durable_tab_id.clone(),
        tab_group_id: None,
        title: durable_pane.title.clone(),
        machine_id: durable_pane.machine_id.clone(),
        layout: TerminalLayoutNode::pane(durable_pane.id.clone()),
    });
    snapshot.terminal_panes.push(durable_pane);

    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot).expect("valid mixed snapshot");
    runtime
        .set_resource_owner(&process_pane_id, None, transient)
        .expect("Root records the projector's transient decision");
    runtime
        .set_restore_target(&process_pane_id, process_target.clone())
        .expect("the exact argv target remains available only in runtime");
    assert_eq!(
        runtime
            .pane(&process_pane_id)
            .and_then(|pane| pane.target.as_ref()),
        Some(&process_target)
    );

    let mut controller = WorkspaceController::from_missing().expect("empty v3 baseline");
    controller
        .synchronize_snapshot(snapshot.clone())
        .expect("complete live workspace keeps the transient tab visible");
    let (transient_tabs, transient_panes) = runtime.transient_ids();
    assert!(transient_tabs.contains(&process_tab_id));
    assert!(transient_panes.contains(&process_pane_id));
    let persisted = without_transient_resources(&snapshot, &transient_tabs, &transient_panes)
        .expect("production persistence filter removes transient target references");
    let request = controller
        .begin_save_projected(persisted)
        .expect("normal CAS entry builds a candidate for the durable tab");
    let request = request.expect("the durable SSH tab remains saveable");
    let candidate = request
        .candidate_document
        .workspace_snapshot()
        .expect("candidate document decodes through the production codec");

    assert_eq!(
        controller.snapshot(),
        &snapshot,
        "UI projection keeps the live tab"
    );
    assert_eq!(candidate.terminal_tabs.len(), 1);
    assert_eq!(candidate.terminal_tabs[0].id(), &durable_tab_id);
    assert_eq!(candidate.terminal_panes.len(), 1);
    assert_eq!(candidate.terminal_panes[0].id, durable_pane_id);
    assert!(
        candidate
            .terminal_panes
            .iter()
            .all(|pane| pane.id != process_pane_id)
    );
    assert!(
        candidate
            .terminal_panes
            .iter()
            .all(|pane| pane.remote_command.is_none())
    );
    assert!(
        !request
            .candidate_document
            .as_value()
            .to_string()
            .contains("arg with spaces"),
        "one-time argv must never enter the v3 save document"
    );
}

/// 确认容器工作目录和 argv 经 shell quote 后仍保留边界，单引号不能注入命令。
#[test]
fn container_exec_quotes_workdir_and_arguments() {
    let pane = container_pane("echo", vec!["hello world".to_owned()]);

    let command = container_exec_command(&pane, Some("/srv/a'b"))
        .expect("UTF-8 command is safely representable");

    assert_eq!(
        command,
        vec![
            "sh".to_owned(),
            "-lc".to_owned(),
            "cd -- '/srv/a'\\''b' && exec 'echo' 'hello world'".to_owned(),
        ]
    );
}

/// 确认控制字符不会进入 shell 命令串，调用方可显示失败并让用户修正目标。
#[test]
fn container_exec_rejects_control_characters() {
    let pane = container_pane("echo\nunsafe", Vec::new());

    let error = container_exec_command(&pane, None).expect_err("control character rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
}

/// 同一旧 targetRef 的 profile/socket 两种组合仍按真实结构化目标区分，且合法 `|` 不被拒绝。
#[test]
fn tmux_binding_uses_structured_target_fields_for_ambiguous_legacy_reference() {
    let legacy_target_ref = "local:work|L:dev".to_owned();
    let profile_with_pipe = TmuxTarget {
        host_id: None,
        profile_id: Some("work|L:dev".to_owned()),
        socket_name: None,
        socket_path: None,
        tmux_path: None,
    };
    let profile_with_socket = TmuxTarget {
        host_id: None,
        profile_id: Some("work".to_owned()),
        socket_name: Some("dev".to_owned()),
        socket_path: None,
        tmux_path: None,
    };
    assert_ne!(profile_with_pipe, profile_with_socket);

    let pane_with_pipe = tmux_pane("work|L:dev");
    let pane_with_socket = tmux_pane("work");
    let binding_without_socket = WorkspaceTmuxBinding {
        target_ref: legacy_target_ref.clone(),
        session_id: "$1".to_owned(),
        session_name: "shell".to_owned(),
        attached_at: "2026-10-17T00:00:00Z".to_owned(),
        socket_name: None,
        socket_path: None,
    };
    let binding_with_socket = WorkspaceTmuxBinding {
        target_ref: legacy_target_ref.clone(),
        session_id: "$1".to_owned(),
        session_name: "shell".to_owned(),
        attached_at: "2026-10-17T00:00:00Z".to_owned(),
        socket_name: Some("dev".to_owned()),
        socket_path: None,
    };

    assert_eq!(
        binding_without_socket.target_ref,
        binding_with_socket.target_ref
    );
    assert!(tmux_binding_matches_target(
        &pane_with_pipe,
        &binding_without_socket,
        &profile_with_pipe,
    ));
    assert!(tmux_binding_matches_target(
        &pane_with_socket,
        &binding_with_socket,
        &profile_with_socket,
    ));
    assert!(!tmux_binding_matches_target(
        &pane_with_pipe,
        &binding_without_socket,
        &profile_with_socket,
    ));
    assert!(!tmux_binding_matches_target(
        &pane_with_socket,
        &binding_with_socket,
        &profile_with_pipe,
    ));
}

/// 模拟 transient PTY 成功 ACK 后的运行态同步，确认 UI 保留完整标签而 CAS 投影剔除它。
#[test]
fn transient_terminal_ack_stays_visible_but_is_filtered_from_saved_snapshot() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let pane = WorkspaceSessionPane {
        id: pane_id.clone(),
        title: "Temporary shell".to_owned(),
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
    };
    let layout = TerminalLayoutNode::pane(pane_id.clone());
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_id.clone()),
        selected_machine_id: None,
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: pane.title.clone(),
            machine_id: pane.machine_id.clone(),
            layout: layout.clone(),
        }],
        terminal_panes: vec![pane.clone()],
        tab_groups: BTreeMap::new(),
    };

    let mut runtime = WorkspaceRuntime::default();
    runtime
        .register_pending_pane(tab_id.clone(), pane.clone())
        .expect("new pane identity is available");
    runtime.register_tab_layout(tab_id.clone(), layout);
    runtime
        .set_resource_owner(&pane_id, None, true)
        .expect("transient ownership is recorded before ACK");
    runtime
        .install_session(
            &pane_id,
            SessionId::new(),
            SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            TerminalSize::default(),
        )
        .expect("successful resource ACK installs a ready pane");

    let mut controller = WorkspaceController::from_missing().expect("empty v3 baseline");
    controller
        .synchronize_snapshot(snapshot.clone())
        .expect("ACK synchronizes the full UI snapshot");
    let visible = controller.tab_projections();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, tab_id);
    assert_eq!(visible[0].kind, WorkspaceTabKind::Terminal);
    assert!(visible[0].active);

    let (transient_tabs, transient_panes) = runtime.transient_ids();
    let persisted = without_transient_resources(&snapshot, &transient_tabs, &transient_panes)
        .expect("transient resources are removed only from the persisted projection");
    assert!(persisted.terminal_tabs.is_empty());
    assert!(persisted.terminal_panes.is_empty());
    assert!(
        controller
            .begin_save_projected(persisted)
            .expect("a transient-only projection is a valid save candidate")
            .is_none()
    );
    assert_eq!(controller.snapshot(), &snapshot);
}

/// 构造本机容器的目标缺省 pane；真实容器身份只由创建后的 SessionTarget 提供。
///
/// @author kongweiguang
fn local_transient_container_snapshot() -> (TabId, PaneId, WorkspaceSessionSnapshot) {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let pane = WorkspaceSessionPane {
        id: pane_id.clone(),
        title: "Local Podman exec".to_owned(),
        machine_id: "machine-local-container".to_owned(),
        mode: WorkspacePaneMode::Container,
        target: None,
        remote_host_id: None,
        container_id: Some("local-container-1".to_owned()),
        profile_id: None,
        shell: Some("podman".to_owned()),
        args: Some(vec!["exec".to_owned(), "local-container-1".to_owned()]),
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Online,
        latency_ms: None,
        local_machine_scope: None,
        output_history: None,
    };
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_id.clone()),
        selected_machine_id: Some(pane.machine_id.clone()),
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: pane.title.clone(),
            machine_id: pane.machine_id.clone(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        terminal_panes: vec![pane],
        tab_groups: BTreeMap::new(),
    };
    (tab_id, pane_id, snapshot)
}

/// 只有精确 transient owner 可在 UI snapshot 中保留本机容器目标缺省 pane，普通新增缺目标仍失败。
///
/// @author kongweiguang
#[test]
fn local_transient_container_sync_requires_exact_owner_and_is_filtered_from_cas() {
    let (tab_id, pane_id, snapshot) = local_transient_container_snapshot();
    snapshot
        .validate()
        .expect("transient local container is a structurally valid in-memory snapshot");

    let mut ordinary = WorkspaceController::from_missing().expect("empty workspace");
    assert_eq!(
        ordinary
            .synchronize_snapshot(snapshot.clone())
            .expect_err("arbitrary targetless pane must remain rejected")
            .public_code(),
        PublicErrorCode::Invalid
    );
    assert!(ordinary.snapshot().terminal_tabs.is_empty());

    for (wrong_tab, wrong_pane, wrong_container) in [
        (
            TabId::new(),
            pane_id.clone(),
            "local-container-1".to_owned(),
        ),
        (
            tab_id.clone(),
            PaneId::new(),
            "local-container-1".to_owned(),
        ),
        (
            tab_id.clone(),
            pane_id.clone(),
            "different-container".to_owned(),
        ),
    ] {
        let mut controller = WorkspaceController::from_missing().expect("empty workspace");
        assert_eq!(
            controller
                .synchronize_snapshot_for_transient_local_container(
                    snapshot.clone(),
                    &wrong_tab,
                    &wrong_pane,
                    &wrong_container,
                )
                .expect_err("binding 属主或目标不精确时仍应拒绝 targetless pane")
                .public_code(),
            PublicErrorCode::Invalid
        );
        assert!(controller.snapshot().terminal_tabs.is_empty());
    }

    let mut controller = WorkspaceController::from_missing().expect("empty workspace");
    controller
        .synchronize_snapshot_for_transient_local_container(
            snapshot,
            &tab_id,
            &pane_id,
            "local-container-1",
        )
        .expect("Root-proven local transient container can be shown in the UI snapshot");
    let visible = controller.tab_projections();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, tab_id);
    assert!(visible[0].active);

    let mut runtime = WorkspaceRuntime::default();
    let transient_pane = controller
        .snapshot()
        .terminal_panes
        .first()
        .expect("pane is in the live controller snapshot")
        .clone();
    runtime
        .register_pending_pane(tab_id.clone(), transient_pane)
        .expect("transient pane identity is available");
    runtime.register_tab_layout(tab_id.clone(), TerminalLayoutNode::pane(pane_id.clone()));
    runtime
        .set_resource_owner(&pane_id, None, true)
        .expect("runtime owns this temporary pane before resource ACK");
    runtime
        .install_session(
            &pane_id,
            SessionId::new(),
            SessionTarget::Container(ContainerSessionTarget {
                host_id: None,
                runtime: ContainerRuntime::Podman,
                container_id: "local-container-1".to_owned(),
                command: vec![OsString::from("podman"), OsString::from("exec")],
            }),
            TerminalSize::default(),
        )
        .expect("the existing transient owner accepts the live container session");
    let (transient_tabs, transient_panes) = runtime.transient_ids();
    let persisted =
        without_transient_resources(controller.snapshot(), &transient_tabs, &transient_panes)
            .expect("the ordinary persistence projector accepts the targetless transient source");
    assert!(persisted.terminal_tabs.is_empty());
    assert!(persisted.terminal_panes.is_empty());
    assert!(persisted.validate().is_ok());
}

/// 仅新建本机 transient ContainerExec 可进入失败清理分支，不能把 SSH 或普通终端误分类。
///
/// @author kongweiguang
#[test]
fn unpublished_container_exec_classifier_requires_local_transient_new_tab() {
    let target = ContainerTarget {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: "local-container-1".to_owned(),
    };
    let local = WorkspaceTerminalOpenKind::ContainerExec {
        target: target.clone(),
        command: vec!["podman".to_owned(), "exec".to_owned()],
    };
    assert_eq!(
        unpublished_transient_local_container_target(&local, true, true),
        Some(target.clone())
    );
    assert_eq!(
        unpublished_transient_local_container_target(&local, false, true),
        None,
        "非 transient 容器操作保留既有 pane 失败语义"
    );
    assert_eq!(
        unpublished_transient_local_container_target(&local, true, false),
        None,
        "已发布或原有 tab 不能被当成未发布预留删除"
    );

    let ssh = WorkspaceTerminalOpenKind::ContainerExec {
        target: ContainerTarget {
            host_id: Some(HostId::new()),
            ..target
        },
        command: vec!["podman".to_owned(), "exec".to_owned()],
    };
    assert_eq!(
        unpublished_transient_local_container_target(&ssh, true, true),
        None,
        "SSH 容器仍由原有 session/pane lifecycle 持有"
    );
}

/// 清理失败先转移到可见 orphan owner，再删除精确的空 transient reservation；已绑定 pane 必须保留。
///
/// @author kongweiguang
#[test]
fn unpublished_local_container_cleanup_removes_only_exact_empty_reservation() {
    let (tab_id, pane_id, snapshot) = local_transient_container_snapshot();
    let target = ContainerTarget {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: "local-container-1".to_owned(),
    };
    let persisted = snapshot.terminal_panes[0].clone();
    let mut runtime = WorkspaceRuntime::default();
    runtime
        .register_pending_pane(tab_id.clone(), persisted)
        .expect("exact local container reservation is unique");
    runtime.register_tab_layout(tab_id.clone(), TerminalLayoutNode::pane(pane_id.clone()));
    runtime
        .set_resource_owner(&pane_id, None, true)
        .expect("transient owner is marked before the async open");

    assert!(
        can_discard_unpublished_transient_local_container_reservation(
            &runtime, &tab_id, &pane_id, &target
        )
    );
    assert!(
        !can_discard_unpublished_transient_local_container_reservation(
            &runtime,
            &tab_id,
            &pane_id,
            &ContainerTarget {
                container_id: "other-container".to_owned(),
                ..target.clone()
            }
        )
    );
    assert!(!discard_unpublished_transient_local_container_reservation(
        &mut runtime,
        &TabId::new(),
        &pane_id,
        &target
    ));
    assert!(discard_unpublished_transient_local_container_reservation(
        &mut runtime,
        &tab_id,
        &pane_id,
        &target
    ));
    assert!(runtime.pane(&pane_id).is_none());
    assert!(runtime.tab_layout(&tab_id).is_none());

    let (tab_id, pane_id, snapshot) = local_transient_container_snapshot();
    let mut runtime = WorkspaceRuntime::default();
    runtime
        .register_pending_pane(tab_id.clone(), snapshot.terminal_panes[0].clone())
        .expect("second exact reservation is unique");
    runtime.register_tab_layout(tab_id.clone(), TerminalLayoutNode::pane(pane_id.clone()));
    runtime
        .set_resource_owner(&pane_id, None, true)
        .expect("transient owner is marked before the async open");
    runtime
        .install_session(
            &pane_id,
            SessionId::new(),
            SessionTarget::Container(ContainerSessionTarget {
                host_id: None,
                runtime: ContainerRuntime::Podman,
                container_id: "local-container-1".to_owned(),
                command: vec![OsString::from("podman"), OsString::from("exec")],
            }),
            TerminalSize::default(),
        )
        .expect("a live resource is installed against the original reservation");
    assert!(!discard_unpublished_transient_local_container_reservation(
        &mut runtime,
        &tab_id,
        &pane_id,
        &target
    ));
    assert!(runtime.pane(&pane_id).is_some());
    assert!(runtime.tab_layout(&tab_id).is_some());
}

/// orphan 转移只接纳同一 UI owner 与容器身份，拒绝迟到或错绑 session 的回执。
///
/// @author kongweiguang
#[test]
fn failed_container_cleanup_requires_exact_session_owner_and_target() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let expected_target = ContainerTarget {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: "local-container-1".to_owned(),
    };
    let session = SessionInfo {
        session_id: SessionId::new(),
        target: SessionTarget::Container(ContainerSessionTarget {
            host_id: None,
            runtime: ContainerRuntime::Podman,
            container_id: "local-container-1".to_owned(),
            command: vec![OsString::from("podman"), OsString::from("exec")],
        }),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Failed,
        exit_status: None,
    };
    assert!(failed_local_container_session_matches(
        &session,
        &tab_id,
        &pane_id,
        &expected_target
    ));
    assert!(!failed_local_container_session_matches(
        &session,
        &TabId::new(),
        &pane_id,
        &expected_target
    ));
    assert!(!failed_local_container_session_matches(
        &session,
        &tab_id,
        &PaneId::new(),
        &expected_target
    ));
    assert!(!failed_local_container_session_matches(
        &session,
        &tab_id,
        &pane_id,
        &ContainerTarget {
            container_id: "other-container".to_owned(),
            ..expected_target
        }
    ));
    assert!(!failed_local_container_session_matches(
        &session,
        &tab_id,
        &pane_id,
        &ContainerTarget {
            runtime: ContainerRuntime::Docker,
            host_id: None,
            container_id: "local-container-1".to_owned(),
        }
    ));
}

/// 待清理 session 先获得独立 owner，原空 pane 才能移除；清理入口仍能按原目标找到 session。
///
/// @author kongweiguang
#[test]
fn failed_transient_container_session_survives_reservation_removal() {
    let (tab_id, pane_id, snapshot) = local_transient_container_snapshot();
    let expected_target = ContainerTarget {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: "local-container-1".to_owned(),
    };
    let mut runtime = WorkspaceRuntime::default();
    runtime
        .register_pending_pane(tab_id.clone(), snapshot.terminal_panes[0].clone())
        .expect("transient pane reservation is available");
    runtime.register_tab_layout(tab_id.clone(), TerminalLayoutNode::pane(pane_id.clone()));
    runtime
        .set_resource_owner(&pane_id, None, true)
        .expect("the transient owner is installed before the create ACK");

    let session_id = SessionId::new();
    let target = SessionTarget::Container(ContainerSessionTarget {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: "local-container-1".to_owned(),
        command: vec![OsString::from("podman"), OsString::from("exec")],
    });
    runtime
        .retain_orphan_cleanup(OrphanCleanupInput {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
            session_id: session_id.clone(),
            target: target.clone(),
            size: TerminalSize::default(),
            snapshot: None,
            status: SharedString::from("cleanup pending"),
            external_lease: None,
            transient: true,
        })
        .expect("the failed close receipt transfers the original owner");

    assert!(discard_unpublished_transient_local_container_reservation(
        &mut runtime,
        &tab_id,
        &pane_id,
        &expected_target
    ));
    assert!(runtime.pane(&pane_id).is_none());
    assert!(runtime.tab_layout(&tab_id).is_none());
    let cleanup = runtime
        .orphan_cleanup(&session_id)
        .expect("orphan owner remains available for explicit cleanup retry");
    assert_eq!(cleanup.tab_id, tab_id);
    assert_eq!(cleanup.pane_id, pane_id);
    assert_eq!(cleanup.target, target);
    assert!(cleanup.transient);
}
