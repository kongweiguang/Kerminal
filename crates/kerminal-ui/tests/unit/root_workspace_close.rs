//! 工作区关闭状态决策回归测试。
//!
//! @author kongweiguang

use crate::features::{
    workspace_controller::{
        WorkspaceController, WorkspaceControllerEffect, WorkspaceResourceOperation,
        WorkspaceRestoreFailure,
    },
    workspace_tabs::{WorkspaceCloseBatchIntent, WorkspaceTabEvent},
};
use crate::root::workspace_close::{
    PendingCloseBatchConfirmationFocus, PendingCloseBatchFocus, PendingCloseTabConfirmationFocus,
    RestoreCloseAction, WorkspaceDeferredCloseIntent, WorkspaceDeferredCloseRequest,
    WorkspaceResourceOperationEntry, cancel_unadmitted_restore_for_close,
    close_survivor_focus_is_current, close_survivor_pane, close_tab_focus_is_current,
    close_tab_focus_owner_is_current, is_last_committed_workspace_pane,
    is_removed_uncommitted_workspace_pane, restore_close_action, take_restore_close_focus,
    take_restore_close_tab_focus,
};
use crate::root::{
    PendingPaneFocusIntent,
    workspace_close::{
        CleanupOwnerSnapshot, CloseSurvivorFocusIntent, CloseTabFocusIntent, SessionCloseRequest,
        WorkspaceCloseResource, close_sessions_until,
    },
    workspace_runtime::{RuntimePanePhase, WorkspaceRuntime},
};
use kerminal_application::sessions::{SessionDeadline, UiSessionBinding};
use kerminal_application::workspace::{
    WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
    WorkspaceSessionDocument, WorkspaceSessionPane, WorkspaceSessionRevision,
    WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionOwner, SessionTarget, TerminalSize},
    workspace::{SplitDirection, TerminalLayoutNode},
};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    future::Future,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll, Waker},
    time::Duration,
};

/// 测试只比较 owner 搬运资料，不创建窗口；None 焦点句柄是本纯状态夹具，不代表生产准入可省略物理来源。
///
/// @author kongweiguang
fn retained_close_focus_fixture(generation: u64) -> CloseSurvivorFocusIntent {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    CloseSurvivorFocusIntent {
        focus: PendingPaneFocusIntent {
            generation,
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
            session_id: Some(session_id.clone()),
            restore_generation: Some(12),
            expected_focus: None,
        },
        binding: UiSessionBinding {
            session_id,
            owner: SessionOwner::Ui { tab_id, pane_id },
            session_generation: 19,
        },
    }
}

/// Restore 票据只携带 tab 身份与用户意图代次；None focus 是无窗口纯状态夹具。
///
/// @author kongweiguang
fn retained_close_tab_focus_fixture(
    tab_id: TabId,
    navigation_generation: u64,
    pane_focus_generation: u64,
) -> CloseTabFocusIntent {
    CloseTabFocusIntent {
        tab_id,
        navigation_generation,
        pane_focus_generation,
        expected_focus: None,
        closing_pane_focus: None,
        closing_search_focus: None,
    }
}

/// 原始来源与目标由首个请求一起占有；旧/失败清理 ACK 不提前移走，晚请求也不能更新 binding/用户代次。
///
/// @author kongweiguang
#[test]
fn deferred_close_focus_is_first_writer_and_exact_cleanup_ack_scoped() {
    let intent = WorkspaceDeferredCloseIntent::Pane {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let original = retained_close_focus_fixture(7);
    let later = retained_close_focus_fixture(8);
    let mut entry = WorkspaceResourceOperationEntry::new("split-owner".to_owned());
    assert_eq!(
        entry.defer_close_with_focus_tickets(intent.clone(), Some(original.clone()), None),
        WorkspaceDeferredCloseRequest::Stored
    );
    assert_eq!(
        entry.defer_close_with_focus_tickets(intent.clone(), Some(later.clone()), None),
        WorkspaceDeferredCloseRequest::Duplicate
    );
    assert_eq!(
        entry.defer_close_with_focus_tickets(
            WorkspaceDeferredCloseIntent::Tab {
                tab_id: TabId::new()
            },
            Some(later),
            None,
        ),
        WorkspaceDeferredCloseRequest::AlreadyPending
    );
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("old-owner", true),
        None
    );
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("split-owner", false),
        None
    );
    assert_eq!(entry.close_focus.as_ref(), Some(&original));
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("split-owner", true),
        Some((intent, Some(original), None))
    );
    assert!(entry.close_focus.is_none());
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("split-owner", true),
        None
    );
}

/// 未提交 preview 只保留已提交 focused pane；精确清理确认才取出原来源，不能把候选树当成已提交关闭。
///
/// @author kongweiguang
#[test]
fn deferred_preview_close_retains_committed_focus_until_cleanup_ack() {
    let source_pane = PaneId::new();
    let preview_pane = PaneId::new();
    let committed = TerminalLayoutNode::pane(source_pane.clone());
    let candidate = committed
        .clone()
        .split_pane(
            &source_pane,
            preview_pane.clone(),
            SplitDirection::Horizontal,
            "preview-focus-split".to_owned(),
            false,
        )
        .expect("valid exact split candidate");
    assert_eq!(
        close_survivor_pane(&committed, &preview_pane, &source_pane),
        None
    );
    assert_eq!(
        close_survivor_pane(&candidate, &preview_pane, &source_pane),
        Some(source_pane.clone())
    );
    let mut original = retained_close_focus_fixture(11);
    original.focus.pane_id = source_pane.clone();
    let tab_id = original.focus.tab_id.clone();
    original.binding.owner = SessionOwner::Ui {
        tab_id: tab_id.clone(),
        pane_id: source_pane,
    };
    let intent = WorkspaceDeferredCloseIntent::Pane {
        tab_id,
        pane_id: preview_pane.clone(),
    };
    let mut entry = WorkspaceResourceOperationEntry::new("preview-owner".to_owned());
    assert_eq!(
        entry.defer_close_with_focus_tickets(intent.clone(), Some(original.clone()), None),
        WorkspaceDeferredCloseRequest::Stored
    );
    assert!(
        entry
            .take_deferred_close_after_cleanup_with_focus("preview-owner", false)
            .is_none()
    );
    assert!(is_removed_uncommitted_workspace_pane(
        Some(&committed),
        false,
        &preview_pane
    ));
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("preview-owner", true),
        Some((intent, Some(original), None))
    );
}

/// 首次来源未知必须一直未知；重复 close 不能借当时的当前焦点补写成新的自动交接权限。
///
/// @author kongweiguang
#[test]
fn deferred_unknown_focus_cannot_be_upgraded_by_duplicate_close() {
    let intent = WorkspaceDeferredCloseIntent::Pane {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let mut entry = WorkspaceResourceOperationEntry::new("unknown-owner".to_owned());
    assert_eq!(
        entry.defer_close_with_focus_tickets(intent.clone(), None, None),
        WorkspaceDeferredCloseRequest::Stored
    );
    assert_eq!(
        entry.defer_close_with_focus_tickets(
            intent.clone(),
            Some(retained_close_focus_fixture(9)),
            None,
        ),
        WorkspaceDeferredCloseRequest::Duplicate
    );
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("unknown-owner", true),
        Some((intent, None, None))
    );
}

/// Restore 等待仅借用已有 entry；重试和旧 ACK 不覆盖来源，精准取走后回收 entry，split owner 则不可误删。
///
/// @author kongweiguang
#[test]
fn restore_close_focus_reuses_exact_owner_without_touching_split_state() {
    let pane_id = PaneId::new();
    let original = retained_close_focus_fixture(3);
    let mut entry = WorkspaceResourceOperationEntry::new("restore-close".to_owned());
    assert!(!entry.retain_restore_close_focus("stale", Some(original.clone())));
    assert!(entry.retain_restore_close_focus("restore-close", Some(original.clone())));
    assert!(
        !entry.retain_restore_close_focus("restore-close", Some(retained_close_focus_fixture(4)))
    );
    assert!(!entry.cancel_requested());
    assert!(!entry.split_measurement_started());
    assert!(entry.deferred_close.is_none());
    let mut owners = HashMap::from([(pane_id.clone(), entry)]);
    assert_eq!(
        take_restore_close_focus(&mut owners, &pane_id, "stale"),
        None
    );
    assert_eq!(
        owners
            .get(&pane_id)
            .expect("original owner retained")
            .close_focus
            .as_ref(),
        Some(&original)
    );
    assert_eq!(
        take_restore_close_focus(&mut owners, &pane_id, "restore-close"),
        Some(original)
    );
    assert!(!owners.contains_key(&pane_id));
    assert_eq!(
        take_restore_close_focus(&mut owners, &pane_id, "restore-close"),
        None
    );
    let mut split = WorkspaceResourceOperationEntry::new("split".to_owned());
    split.defer_close_with_focus_tickets(
        WorkspaceDeferredCloseIntent::Tab {
            tab_id: TabId::new(),
        },
        Some(retained_close_focus_fixture(5)),
        None,
    );
    owners.insert(pane_id.clone(), split);
    assert_eq!(
        take_restore_close_focus(&mut owners, &pane_id, "split"),
        None
    );
    assert!(owners.contains_key(&pane_id));
}

/// Restore 等待的 CloseTab 焦点票据必须与 operation owner 精确绑定，旧代次不可取走或覆盖。
///
/// @author kongweiguang
#[test]
fn restore_close_tab_focus_reuses_exact_owner_without_replacing_intent() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let original = retained_close_tab_focus_fixture(tab_id, 12, 8);
    let mut entry = WorkspaceResourceOperationEntry::new("restore-tab-close".to_owned());
    assert!(!entry.retain_restore_close_tab_focus("stale", Some(original.clone())));
    assert!(entry.retain_restore_close_tab_focus("restore-tab-close", Some(original.clone())));
    assert!(!entry.retain_restore_close_tab_focus(
        "restore-tab-close",
        Some(retained_close_tab_focus_fixture(TabId::new(), 13, 9)),
    ));
    let mut owners = HashMap::from([(pane_id.clone(), entry)]);
    assert_eq!(
        take_restore_close_tab_focus(&mut owners, &pane_id, "stale"),
        None
    );
    assert_eq!(
        owners
            .get(&pane_id)
            .expect("exact operation owner retained")
            .close_tab_focus
            .as_ref(),
        Some(&original)
    );
    assert_eq!(
        take_restore_close_tab_focus(&mut owners, &pane_id, "restore-tab-close"),
        Some(original)
    );
    assert!(!owners.contains_key(&pane_id));
}

/// Split 延迟关闭只把首个 CloseTab 的原始意图代次交给 ACK 回放，重复请求与失败清理不能替换或消费它。
///
/// @author kongweiguang
#[test]
fn deferred_close_tab_focus_is_retained_until_exact_cleanup_ack() {
    let tab_id = TabId::new();
    let original = retained_close_tab_focus_fixture(tab_id.clone(), 17, 9);
    let later = retained_close_tab_focus_fixture(tab_id.clone(), 18, 10);
    let intent = WorkspaceDeferredCloseIntent::Tab { tab_id };
    let mut entry = WorkspaceResourceOperationEntry::new("deferred-tab-close".to_owned());

    assert_eq!(
        entry.defer_close_with_focus_tickets(intent.clone(), None, Some(original.clone())),
        WorkspaceDeferredCloseRequest::Stored
    );
    assert_eq!(
        entry.defer_close_with_focus_tickets(intent.clone(), None, Some(later)),
        WorkspaceDeferredCloseRequest::Duplicate
    );
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("stale", true),
        None
    );
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("deferred-tab-close", false),
        None
    );
    assert_eq!(
        entry.take_deferred_close_after_cleanup_with_focus("deferred-tab-close", true),
        Some((intent, None, Some(original)))
    );
}

/// 原用户意图与物理焦点未变时才允许交接；导航、模态或退出阻断不因关闭 ACK 被绕过。
///
/// @author kongweiguang
#[test]
fn close_survivor_focus_requires_unchanged_user_intent_and_focus_gate() {
    let binding = UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 7,
    };
    assert!(close_survivor_focus_is_current(
        4,
        4,
        true,
        &binding,
        Some(&binding),
        Some(2),
        Some(2),
    ));
    for (current_generation, focus_allowed, current_restore_generation) in [
        (5, true, Some(2)),
        (4, false, Some(2)),
        (4, true, Some(3)),
        (4, true, None),
    ] {
        assert!(!close_survivor_focus_is_current(
            4,
            current_generation,
            focus_allowed,
            &binding,
            Some(&binding),
            Some(2),
            current_restore_generation,
        ));
    }
}

/// Tab close ACK 只能在同一关闭目标、导航代次、pane 焦点代次和物理焦点持续成立时转交焦点。
///
/// @author kongweiguang
#[test]
fn close_tab_focus_rejects_stale_target_navigation_and_focus() {
    let tab_id = TabId::new();
    let intent = retained_close_tab_focus_fixture(tab_id.clone(), 22, 6);
    assert!(close_tab_focus_is_current(&intent, &tab_id, 22, 6, true));
    assert!(!close_tab_focus_is_current(
        &intent,
        &TabId::new(),
        22,
        6,
        true,
    ));
    assert!(!close_tab_focus_is_current(&intent, &tab_id, 23, 6, true));
    assert!(!close_tab_focus_is_current(&intent, &tab_id, 22, 7, true));
    assert!(!close_tab_focus_is_current(&intent, &tab_id, 22, 6, false));
}

/// 搜索框交接只接受原 session 的精确输入句柄或其销毁后的无焦点态，不得接管其它 Editor。
/// 退出 pane 已退休 input handler；其 FocusHandle 退树后的 App 回退仍须保留精确原句柄所有权。
///
/// @author kongweiguang
#[test]
fn close_tab_exited_search_focus_survives_without_input_handler_or_rendered_context() {
    use crate::root::keybindings::KeybindingFocusScope;

    let search_focus = String::from("closing-session-search");
    let root_focus = String::from("root");
    let later_editor_focus = String::from("other-editor");

    // `closing_pane_focus: None` models shell exit retiring its live input handler while the history search stays mounted.
    assert!(close_tab_focus_owner_is_current(
        Some(&search_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Editor,
    ));
    assert!(close_tab_focus_owner_is_current(
        Some(&search_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::App,
    ));
    assert!(close_tab_focus_owner_is_current(
        Some(&search_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Terminal,
    ));
    assert!(close_tab_focus_owner_is_current(
        Some(&search_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Workspace,
    ));
    assert!(close_tab_focus_owner_is_current(
        None,
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Editor,
    ));
    assert!(!close_tab_focus_owner_is_current(
        None,
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Modal,
    ));
    assert!(close_tab_focus_owner_is_current(
        Some(&root_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Workspace,
    ));
    assert!(close_tab_focus_owner_is_current(
        Some(&root_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::App,
    ));
    assert!(!close_tab_focus_owner_is_current(
        Some(&later_editor_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Editor,
    ));
    assert!(!close_tab_focus_owner_is_current(
        Some(&later_editor_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::App,
    ));
    assert!(!close_tab_focus_owner_is_current(
        Some(&search_focus),
        Some(&search_focus),
        None,
        Some(&search_focus),
        &root_focus,
        KeybindingFocusScope::Modal,
    ));
    assert!(!close_tab_focus_owner_is_current(
        Some(&later_editor_focus),
        Some(&later_editor_focus),
        None,
        None,
        &root_focus,
        KeybindingFocusScope::Editor,
    ));
}

/// 已退役的 pane focus handle 退树后可接受 App fallback，但不能把它扩展到后来取得焦点的控件或 Modal。
///
/// @author kongweiguang
#[test]
fn close_tab_retired_pane_focus_accepts_only_its_app_fallback() {
    use crate::root::keybindings::KeybindingFocusScope;

    let pane_focus = String::from("closing-pane");
    let later_editor_focus = String::from("other-editor");
    let root_focus = String::from("root");

    assert!(close_tab_focus_owner_is_current(
        Some(&pane_focus),
        Some(&pane_focus),
        Some(&pane_focus),
        None,
        &root_focus,
        KeybindingFocusScope::App,
    ));
    assert!(!close_tab_focus_owner_is_current(
        Some(&later_editor_focus),
        Some(&pane_focus),
        Some(&pane_focus),
        None,
        &root_focus,
        KeybindingFocusScope::App,
    ));
    assert!(!close_tab_focus_owner_is_current(
        Some(&pane_focus),
        Some(&pane_focus),
        Some(&pane_focus),
        None,
        &root_focus,
        KeybindingFocusScope::Modal,
    ));
}

/// 只有同一确认请求确认后才能消费弹层前的焦点；取消、迟到回调和无资格快照都不能补捕获。
///
/// @author kongweiguang
#[test]
fn close_confirmation_focus_ticket_is_request_scoped_and_preserves_ineligible_capture() {
    let tab_id = TabId::new();
    let focus = retained_close_tab_focus_fixture(tab_id.clone(), 22, 6);
    let mut pending =
        PendingCloseTabConfirmationFocus::new(41, tab_id.clone(), Some(focus.clone()));

    assert!(!pending.confirm(40, &tab_id));
    assert_eq!(pending.take_confirmed(41, &tab_id), None);
    assert!(!pending.cancel(41, &TabId::new()));
    assert!(pending.confirm(41, &tab_id));
    assert_eq!(pending.take_confirmed(42, &tab_id), None);
    assert_eq!(pending.take_confirmed(41, &tab_id), Some(Some(focus)));
    assert_eq!(pending.take_confirmed(41, &tab_id), None);

    let no_focus_tab = TabId::new();
    let mut no_focus = PendingCloseTabConfirmationFocus::new(7, no_focus_tab.clone(), None);
    assert!(no_focus.confirm(7, &no_focus_tab));
    assert_eq!(no_focus.take_confirmed(7, &no_focus_tab), Some(None));

    let cancelled_tab = TabId::new();
    let cancelled = PendingCloseTabConfirmationFocus::new(9, cancelled_tab.clone(), None);
    assert!(cancelled.cancel(9, &cancelled_tab));
    assert!(!cancelled.cancel(10, &cancelled_tab));
}

/// 批量弹层回执必须匹配原请求和冻结目标；未确认、取消或迟到确认不能产生可消费焦点。
///
/// @author kongweiguang
#[test]
fn close_batch_confirmation_focus_ticket_is_intent_scoped_and_one_shot() {
    let active_tab = TabId::new();
    let other_tab = TabId::new();
    let intent = WorkspaceCloseBatchIntent::Group {
        group_id: "group-a".to_owned(),
        tab_ids: vec![active_tab.clone(), other_tab],
    };
    let focus = retained_close_tab_focus_fixture(active_tab.clone(), 22, 6);
    let mut pending = PendingCloseBatchConfirmationFocus::new(
        41,
        intent.clone(),
        Some(active_tab.clone()),
        Some(focus.clone()),
    );
    let changed_intent = WorkspaceCloseBatchIntent::Group {
        group_id: "group-a".to_owned(),
        tab_ids: vec![active_tab.clone()],
    };

    assert!(!pending.confirm(40, &intent));
    assert!(!pending.confirm(41, &changed_intent));
    assert_eq!(pending.take_confirmed(41, &intent), None);
    let cancelled = PendingCloseBatchConfirmationFocus::new(42, intent.clone(), None, None);
    assert!(cancelled.cancel(42, &intent));
    assert!(!cancelled.cancel(43, &intent));

    assert!(pending.confirm(41, &intent));
    assert_eq!(pending.take_confirmed(41, &changed_intent), None);
    assert_eq!(
        pending.take_confirmed(41, &intent),
        Some((Some(active_tab), Some(focus)))
    );
    assert_eq!(pending.take_confirmed(41, &intent), None);
}

/// 失败重试保留首次焦点票据；只有目标 CloseTab 成功且整批完成才可一次性交接。
///
/// @author kongweiguang
#[test]
fn close_batch_focus_survives_failure_and_rejects_stale_or_unclosed_targets() {
    let active_tab = TabId::new();
    let other_tab = TabId::new();
    let intent = WorkspaceCloseBatchIntent::ToRight {
        anchor_tab_id: TabId::new(),
        tab_ids: vec![active_tab.clone(), other_tab.clone()],
    };
    let focus = retained_close_tab_focus_fixture(active_tab.clone(), 22, 6);
    let mut pending = PendingCloseBatchFocus::new(
        intent.clone(),
        Some(active_tab.clone()),
        Some(focus.clone()),
    );
    let stale_intent = WorkspaceCloseBatchIntent::ToRight {
        anchor_tab_id: intent.tab_ids()[0].clone(),
        tab_ids: intent.tab_ids().to_vec(),
    };

    pending.note_tab_closed(&other_tab);
    assert_eq!(pending.take_after_successful_batch(&stale_intent), None);
    pending.note_tab_closed(&active_tab);
    assert_eq!(
        pending.take_after_successful_batch(&intent),
        Some(Some(focus))
    );
    assert_eq!(pending.take_after_successful_batch(&intent), None);

    let mut target_not_closed = PendingCloseBatchFocus::new(
        intent.clone(),
        Some(active_tab.clone()),
        Some(retained_close_tab_focus_fixture(active_tab.clone(), 23, 7)),
    );
    target_not_closed.note_tab_closed(&other_tab);
    assert_eq!(
        target_not_closed.take_after_successful_batch(&intent),
        Some(None)
    );

    let filtered_intent = WorkspaceCloseBatchIntent::Group {
        group_id: "group-b".to_owned(),
        tab_ids: vec![other_tab.clone()],
    };
    let mut filtered = PendingCloseBatchFocus::new(
        filtered_intent.clone(),
        Some(active_tab.clone()),
        Some(retained_close_tab_focus_fixture(active_tab.clone(), 24, 8)),
    );
    filtered.note_tab_closed(&active_tab);
    assert_eq!(
        filtered.take_after_successful_batch(&filtered_intent),
        Some(None)
    );
}

/// 同名 pane/session 的晚回执仍须核对完整 owner 与 session generation，不能把输入交给替代资源。
///
/// @author kongweiguang
#[test]
fn close_survivor_focus_rejects_replaced_binding_and_missing_owner() {
    let binding = UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 7,
    };
    let mut other_session = binding.clone();
    other_session.session_id = SessionId::new();
    let mut other_owner = binding.clone();
    other_owner.owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let mut other_generation = binding.clone();
    other_generation.session_generation += 1;
    for current in [
        None,
        Some(&other_session),
        Some(&other_owner),
        Some(&other_generation),
    ] {
        assert!(!close_survivor_focus_is_current(
            4, 4, true, &binding, current, None, None,
        ));
    }
}

/// 固定 reducer 的真实幸存顺序，焦点交接只核对已成功提交的 focused pane，不在 ACK 时另选目标。
///
/// @author kongweiguang
#[test]
fn focused_pane_close_commits_the_exact_first_survivor() {
    let (snapshot, tab_id, pane_ids) = close_fixture();
    let closed_pane = pane_ids[0].clone();
    let expected_survivor = snapshot.terminal_tabs[0]
        .terminal_layout()
        .expect("terminal fixture")
        .pane_ids()
        .into_iter()
        .find(|pane| pane != &closed_pane)
        .expect("fixture has survivors");
    assert_eq!(
        close_survivor_pane(
            snapshot.terminal_tabs[0]
                .terminal_layout()
                .expect("terminal fixture"),
            &closed_pane,
            &closed_pane,
        ),
        Some(expected_survivor.clone())
    );
    let mut document = WorkspaceSessionDocument::empty_v3();
    document
        .update_known_fields(&snapshot)
        .expect("persist fixture");
    let mut controller = WorkspaceController::from_loaded(
        document,
        WorkspaceSessionRevision::new("close-focus-base"),
    )
    .expect("valid controller");
    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_pane_event(
            &tab_id,
            crate::features::workspace_panes::WorkspacePaneEvent::Close(closed_pane.clone()),
        )
        .expect("begin original close")
    else {
        panic!("close requires a resource receipt")
    };
    assert_eq!(
        controller.snapshot().focused_pane_id.as_ref(),
        Some(&closed_pane)
    );
    controller
        .complete_resource_operation(operation.operation_id(), Ok(()))
        .expect("original close succeeded");
    assert_eq!(controller.snapshot().active_tab_id.as_ref(), Some(&tab_id));
    assert_eq!(
        controller.snapshot().focused_pane_id,
        Some(expected_survivor)
    );
}

/// 键盘可落在未逻辑选中的 pane 关闭按钮；关闭后保留原 focused pane，不能无条件改选第一个 survivor。
///
/// @author kongweiguang
#[test]
fn closing_unfocused_pane_keeps_the_exact_reducer_focused_survivor() {
    let (mut snapshot, tab_id, pane_ids) = close_fixture();
    let closed_pane = pane_ids[1].clone();
    let expected_survivor = pane_ids[2].clone();
    snapshot.focused_pane_id = Some(expected_survivor.clone());
    let layout = snapshot.terminal_tabs[0]
        .terminal_layout()
        .expect("terminal fixture");
    assert_ne!(layout.pane_ids().first(), Some(&expected_survivor));
    assert_eq!(
        close_survivor_pane(layout, &closed_pane, &expected_survivor),
        Some(expected_survivor.clone())
    );
    assert_eq!(
        close_survivor_pane(layout, &PaneId::new(), &expected_survivor),
        None
    );
    assert_eq!(
        close_survivor_pane(layout, &closed_pane, &PaneId::new()),
        None
    );
    let mut document = WorkspaceSessionDocument::empty_v3();
    document
        .update_known_fields(&snapshot)
        .expect("persist fixture");
    let mut controller = WorkspaceController::from_loaded(
        document,
        WorkspaceSessionRevision::new("unfocused-close-base"),
    )
    .expect("valid controller");
    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_pane_event(
            &tab_id,
            crate::features::workspace_panes::WorkspacePaneEvent::Close(closed_pane),
        )
        .expect("begin original close")
    else {
        panic!("close requires a resource receipt")
    };
    controller
        .complete_resource_operation(operation.operation_id(), Ok(()))
        .expect("original close succeeded");
    assert_eq!(
        controller.snapshot().focused_pane_id,
        Some(expected_survivor)
    );
}

/// 关闭只关联来源、候选或所属 Tab；源的最后一个已提交 pane 先走 Tab 确认，候选本身不算最后 pane。
///
/// @author kongweiguang
#[test]
fn split_close_target_and_last_committed_pane_decisions_are_exact() {
    let split_tab_id = TabId::new();
    let unrelated_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let candidate_pane_id = PaneId::new();
    let unrelated_pane_id = PaneId::new();
    let committed_layout = TerminalLayoutNode::pane(source_pane_id.clone());

    assert!(is_last_committed_workspace_pane(
        Some(&committed_layout),
        &source_pane_id
    ));
    assert!(
        !is_last_committed_workspace_pane(Some(&committed_layout), &candidate_pane_id),
        "the pending candidate is not part of the committed one-pane layout"
    );
    assert!(
        !is_removed_uncommitted_workspace_pane(Some(&committed_layout), true, &candidate_pane_id),
        "the candidate close must reach split cancellation while its runtime owner exists"
    );
    assert!(is_removed_uncommitted_workspace_pane(
        Some(&committed_layout),
        false,
        &candidate_pane_id
    ));

    let source_close = WorkspaceDeferredCloseIntent::Pane {
        tab_id: split_tab_id.clone(),
        pane_id: source_pane_id.clone(),
    };
    let candidate_close = WorkspaceDeferredCloseIntent::Pane {
        tab_id: split_tab_id.clone(),
        pane_id: candidate_pane_id.clone(),
    };
    let other_pane_close = WorkspaceDeferredCloseIntent::Pane {
        tab_id: split_tab_id.clone(),
        pane_id: unrelated_pane_id,
    };
    let other_tab_close = WorkspaceDeferredCloseIntent::Pane {
        tab_id: unrelated_tab_id.clone(),
        pane_id: source_pane_id.clone(),
    };
    assert!(source_close.targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id));
    assert!(candidate_close.targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id));
    assert!(!other_pane_close.targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id));
    assert!(!other_tab_close.targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id));
    assert!(
        WorkspaceDeferredCloseIntent::Tab {
            tab_id: split_tab_id.clone(),
        }
        .targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id)
    );
    assert!(
        !WorkspaceDeferredCloseIntent::Tab {
            tab_id: unrelated_tab_id.clone(),
        }
        .targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id)
    );
    assert!(
        WorkspaceDeferredCloseIntent::Batch(
            crate::features::workspace_tabs::WorkspaceCloseBatchIntent::Group {
                group_id: "g".to_owned(),
                tab_ids: vec![split_tab_id.clone()],
            },
        )
        .targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id)
    );
    assert!(
        !WorkspaceDeferredCloseIntent::Batch(
            crate::features::workspace_tabs::WorkspaceCloseBatchIntent::Group {
                group_id: "other".to_owned(),
                tab_ids: vec![unrelated_tab_id],
            },
        )
        .targets_split(&split_tab_id, &source_pane_id, &candidate_pane_id)
    );
}

/// 测量前取消必须立即关闭准入，并保留精确的原始关闭目标。
///
/// @author kongweiguang
#[test]
fn split_close_before_measurement_withdraws_admission_and_keeps_one_intent() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let intent = WorkspaceDeferredCloseIntent::Pane { tab_id, pane_id };
    let mut entry = WorkspaceResourceOperationEntry::new("split-before-measurement".to_owned());

    assert!(!entry.split_measurement_started());
    assert!(!entry.try_admit_split_measurement("replacement-op", 6));
    assert_eq!(
        entry.defer_close(intent.clone()),
        WorkspaceDeferredCloseRequest::Stored
    );
    assert!(entry.cancel_requested());
    assert!(!entry.try_admit_split_measurement("split-before-measurement", 7));
    assert_eq!(
        entry.defer_close(intent.clone()),
        WorkspaceDeferredCloseRequest::Duplicate
    );
    assert_eq!(
        entry.defer_close(WorkspaceDeferredCloseIntent::Tab {
            tab_id: TabId::new(),
        }),
        WorkspaceDeferredCloseRequest::AlreadyPending,
        "a later close cannot replace the user's retained target"
    );
    assert_eq!(
        entry
            .take_deferred_close_after_cleanup_with_focus("stale-split", true)
            .map(|(intent, _, _)| intent),
        None
    );
    assert_eq!(
        entry
            .take_deferred_close_after_cleanup_with_focus("split-before-measurement", true)
            .map(|(intent, _, _)| intent),
        Some(intent)
    );
    assert_eq!(
        entry
            .take_deferred_close_after_cleanup_with_focus("split-before-measurement", true)
            .map(|(intent, _, _)| intent),
        None,
        "the original close is replayed at most once"
    );
}

/// 已准入恢复在清理确认前继续由原操作持有；失败或未知结果不能提前重放关闭。
///
/// @author kongweiguang
#[test]
fn admitted_split_close_waits_for_cleanup_and_consumes_the_original_batch_once() {
    let tab_id = TabId::new();
    let intent = WorkspaceDeferredCloseIntent::Batch(
        crate::features::workspace_tabs::WorkspaceCloseBatchIntent::ToRight {
            anchor_tab_id: TabId::new(),
            tab_ids: vec![tab_id],
        },
    );
    let mut entry = WorkspaceResourceOperationEntry::new("split-admitted".to_owned());

    assert!(entry.try_admit_split_measurement("split-admitted", 11));
    assert!(entry.split_measurement_started());
    assert_eq!(
        entry.defer_close(intent.clone()),
        WorkspaceDeferredCloseRequest::Stored
    );
    assert!(!entry.try_admit_split_measurement("split-admitted", 12));
    assert!(!entry.reset_split_measurement_for_retry("split-admitted"));
    assert_eq!(
        entry
            .take_deferred_close_after_cleanup_with_focus("split-admitted", false)
            .map(|(intent, _, _)| intent),
        None,
        "orphan 清理失败或结果未知时，资源 owner 与关闭意图都必须保留"
    );
    assert_eq!(
        entry
            .take_deferred_close_after_cleanup_with_focus("split-admitted", true)
            .map(|(intent, _, _)| intent),
        Some(intent)
    );
    assert_eq!(
        entry
            .take_deferred_close_after_cleanup_with_focus("split-admitted", true)
            .map(|(intent, _, _)| intent),
        None,
        "a duplicate stale ACK cannot replay the batch twice"
    );
}

/// 显式重试仅在调用方核实旧失败资源已释放后，才重开同一操作的测量准入。
///
/// @author kongweiguang
#[test]
fn failed_split_retry_resets_only_the_exact_unowned_operation_measurement() {
    let mut entry = WorkspaceResourceOperationEntry::new("split-retry".to_owned());

    assert!(entry.try_admit_split_measurement("split-retry", 19));
    assert!(!entry.reset_split_measurement_for_retry("replacement-op"));
    assert!(entry.reset_split_measurement_for_retry("split-retry"));
    assert!(!entry.split_measurement_started());
    assert!(entry.try_admit_split_measurement("split-retry", 20));

    let intent = WorkspaceDeferredCloseIntent::Tab {
        tab_id: TabId::new(),
    };
    assert_eq!(
        entry.defer_close(intent),
        WorkspaceDeferredCloseRequest::Stored
    );
    assert!(!entry.reset_split_measurement_for_retry("split-retry"));
}

/// 关闭请求遇到尚未确认的创建时必须等待 ACK，不能先删 pane 或提交候选。
#[test]
fn pending_restore_waits_for_resource_acknowledgement() {
    assert_eq!(
        restore_close_action(RuntimePanePhase::RestorePending, false, true),
        RestoreCloseAction::WaitForAcknowledgement
    );
}

/// 清理失败的原 session 可重试关闭，而不是通过创建替代会话掩盖未完成资源。
#[test]
fn close_failed_session_is_retried_in_place() {
    assert_eq!(
        restore_close_action(RuntimePanePhase::CloseFailed, true, false),
        RestoreCloseAction::CloseSession
    );
}

/// 其它关闭操作持有的资源不被重复关闭，空失败 pane 则可在 ACK 后移除。
#[test]
fn closing_and_failed_empty_panes_are_distinguished() {
    assert_eq!(
        restore_close_action(RuntimePanePhase::Closing, true, false),
        RestoreCloseAction::Reject
    );
    assert_eq!(
        restore_close_action(RuntimePanePhase::RestoreFailed, false, false),
        RestoreCloseAction::RemoveEmptyPane
    );
}

/// 首显前的持久占位没有任务 owner；取消后仍等待 ACK 的原准入则不能借无 session 被删。
///
/// @author kongweiguang
#[test]
fn never_admitted_pending_pane_is_empty_but_cancelled_admission_still_waits() {
    assert_eq!(
        restore_close_action(RuntimePanePhase::RestorePending, false, false),
        RestoreCloseAction::RemoveEmptyPane
    );
    assert_eq!(
        restore_close_action(RuntimePanePhase::RestorePending, false, true),
        RestoreCloseAction::WaitForAcknowledgement
    );
    assert_eq!(
        restore_close_action(RuntimePanePhase::RestorePending, true, false),
        RestoreCloseAction::WaitForAcknowledgement
    );
}

/// 混合标签关闭失败时成对保留未准入占位的 Cancelled 投影，重试关闭只处理原 ready owner。
///
/// @author kongweiguang
#[test]
fn mixed_ready_and_deferred_tab_close_retries_without_waiting_for_unstarted_panes() {
    let (snapshot, tab_id, pane_ids) = close_fixture();
    let mut document = WorkspaceSessionDocument::empty_v3();
    document
        .update_known_fields(&snapshot)
        .expect("persisted fixture");
    let mut controller = WorkspaceController::from_loaded(
        document,
        WorkspaceSessionRevision::new("mixed-close-base"),
    )
    .expect("loaded panes start pending without admitting any task");
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot).expect("valid runtime");
    let session_id = SessionId::new();
    runtime
        .install_session(
            &pane_ids[0],
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .expect("only the first pane was ever opened");
    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_tab_event(WorkspaceTabEvent::Close(tab_id))
        .expect("begin mixed tab close")
    else {
        panic!("terminal close must retain its resource operation");
    };
    assert_eq!(
        restore_close_action(RuntimePanePhase::Ready, true, true),
        RestoreCloseAction::CloseSession
    );
    for pane_id in &pane_ids[1..] {
        assert!(cancel_unadmitted_restore_for_close(
            &mut runtime,
            &mut controller,
            pane_id,
        ));
        assert_eq!(
            runtime.pane(pane_id).map(|pane| pane.phase),
            Some(RuntimePanePhase::RestoreFailed)
        );
        assert_eq!(
            controller.restore_status(
                &crate::features::workspace_controller::WorkspaceRestoreItemId::Pane(
                    pane_id.clone()
                )
            ),
            Some(
                crate::features::workspace_controller::WorkspaceRestoreStatus::Failed(
                    WorkspaceRestoreFailure::Cancelled
                )
            )
        );
    }
    assert!(
        controller
            .begin_save()
            .expect("query quit save gate")
            .is_none()
    );
    runtime
        .begin_close(&pane_ids[0])
        .expect("close original owner");
    runtime
        .mark_close_failed(&pane_ids[0], &session_id)
        .expect("original close failed");
    controller
        .complete_resource_operation(
            operation.operation_id(),
            Err(WorkspaceRestoreFailure::ResourceFailure),
        )
        .expect("same close remains retryable");
    for pane_id in &pane_ids[1..] {
        let pane = runtime
            .pane(pane_id)
            .expect("deferred pane remains visible");
        assert_eq!(
            restore_close_action(pane.phase, pane.session_id.is_some(), true),
            RestoreCloseAction::RemoveEmptyPane,
            "a cancellation generation tombstone cannot turn an unstarted pane into an ACK owner"
        );
    }
    assert_eq!(
        controller.retry_pending_resource_operation(),
        Some(operation.clone())
    );
    runtime
        .begin_close(&pane_ids[0])
        .expect("retry original owner");
    runtime
        .remove_closed_session(&pane_ids[0], &session_id)
        .expect("original resource ACK");
    for pane_id in &pane_ids[1..] {
        assert!(
            runtime
                .remove_empty_pane(pane_id)
                .expect("remove deferred placeholder")
        );
    }
    controller
        .complete_resource_operation(operation.operation_id(), Ok(()))
        .expect("commit original close only after resource ACK");
    assert!(runtime.panes().next().is_none());
    assert!(controller.snapshot().terminal_tabs.is_empty());
    assert!(controller.pending_resource_operation().is_none());
    assert!(
        controller
            .begin_save()
            .expect("quit save is no longer blocked")
            .is_some()
    );
}

/// 组装多级终端关闭用的 typed 快照，确保控制器和运行态共享稳定 pane 身份。
fn close_fixture() -> (WorkspaceSessionSnapshot, TabId, Vec<PaneId>) {
    let tab_id = TabId::new();
    let panes = (0..3)
        .map(|_| WorkspaceSessionPane {
            id: PaneId::new(),
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
        })
        .collect::<Vec<_>>();
    let mut layout = TerminalLayoutNode::pane(panes[0].id.clone());
    for (index, pane) in panes.iter().enumerate().skip(1) {
        layout = layout
            .split_pane(
                &panes[0].id,
                pane.id.clone(),
                SplitDirection::Horizontal,
                format!("close-split-{index}"),
                false,
            )
            .expect("fixture layout remains valid");
    }
    let pane_ids = panes.iter().map(|pane| pane.id.clone()).collect();
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: panes.first().map(|pane| pane.id.clone()),
        selected_machine_id: Some("local".to_owned()),
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "Local".to_owned(),
            machine_id: "local".to_owned(),
            layout,
        }],
        terminal_panes: panes,
        tab_groups: BTreeMap::new(),
    };
    (snapshot, tab_id, pane_ids)
}

/// 生成明确本地目标，关闭失败后 Runtime 才能继续持有原资源 owner。
fn local_target() -> SessionTarget {
    SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("configured-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    })
}

/// 将仅含立即完成 future 的生产 close helper 驱动到结束，不引入 UI 测试 runtime。
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = TaskContext::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// 构造与 Root 提交回执时相同的清理 owner 快照。
fn close_request(tab_id: &TabId, pane_id: &PaneId, session_id: SessionId) -> SessionCloseRequest {
    SessionCloseRequest {
        resource: WorkspaceCloseResource::Pane(pane_id.clone()),
        session_id,
        owner: CleanupOwnerSnapshot {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
            target: Some(local_target()),
            size: TerminalSize::default(),
            snapshot: None,
            external_lease: None,
            transient: false,
        },
    }
}

/// 验证关闭按请求顺序共享绝对 deadline、失败不跳过后续目标，并在原 reducer/runtime 操作中重试。
#[test]
fn batch_close_keeps_shared_deadline_and_retries_partial_ack_in_place() {
    let (snapshot, tab_id, pane_ids) = close_fixture();
    snapshot.validate().expect("close fixture is valid");
    let mut controller = WorkspaceController::from_missing().expect("missing controller");
    controller
        .synchronize_snapshot(snapshot.clone())
        .expect("install the active typed snapshot");
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot).expect("valid runtime snapshot");
    let session_ids = pane_ids
        .iter()
        .map(|pane_id| {
            let session_id = SessionId::new();
            runtime
                .install_session(
                    pane_id,
                    session_id.clone(),
                    local_target(),
                    TerminalSize::default(),
                )
                .expect("bind each real session to its pane");
            session_id
        })
        .collect::<Vec<_>>();

    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_tab_event(WorkspaceTabEvent::Close(tab_id.clone()))
        .expect("close starts a resource operation")
    else {
        panic!("terminal tab close must wait for resource acknowledgements");
    };
    let (operation_id, operation_panes) = match &operation {
        WorkspaceResourceOperation::CloseTab {
            operation_id,
            pane_ids,
            ..
        } => (operation_id.clone(), pane_ids.clone()),
        _ => panic!("closing a terminal tab must produce CloseTab"),
    };
    let mut first_requests = Vec::new();
    for pane_id in &operation_panes {
        let index = pane_ids
            .iter()
            .position(|candidate| candidate == pane_id)
            .expect("operation pane belongs to the runtime snapshot");
        let session_id = runtime
            .begin_close(pane_id)
            .expect("ready session enters closing state");
        assert_eq!(session_id, session_ids[index]);
        first_requests.push(close_request(&tab_id, pane_id, session_id));
    }

    let failed_session = session_ids[1].clone();
    let failed_session_for_close = failed_session.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first_deadline = SessionDeadline::now() + Duration::from_secs(5);
    let observed_calls = Arc::clone(&calls);
    let expected_close_order = first_requests
        .iter()
        .map(|request| request.session_id.clone())
        .collect::<Vec<_>>();
    let first_results = block_on(close_sessions_until(
        first_requests,
        first_deadline,
        move |session_id, deadline| {
            let calls = Arc::clone(&observed_calls);
            let failed_session = failed_session_for_close.clone();
            async move {
                calls
                    .lock()
                    .expect("call log lock")
                    .push((session_id.clone(), deadline));
                if session_id == failed_session {
                    Err("simulated close failure".to_owned())
                } else {
                    Ok(())
                }
            }
        },
    ));

    let observed = calls.lock().expect("call log lock");
    assert_eq!(
        observed.len(),
        3,
        "a failed close cannot skip later sessions"
    );
    assert_eq!(
        observed
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        expected_close_order,
        "close preserves the controller-provided layout traversal order"
    );
    assert!(
        observed
            .iter()
            .all(|(_, deadline)| *deadline == first_deadline)
    );
    drop(observed);

    for (request, result) in first_results {
        let WorkspaceCloseResource::Pane(pane_id) = request.resource else {
            panic!("fixture contains pane resources only");
        };
        match result {
            Ok(()) => assert!(
                runtime
                    .remove_closed_session(&pane_id, &request.session_id)
                    .expect("successful close removes its original owner")
            ),
            Err(_) => assert!(
                runtime
                    .mark_close_failed(&pane_id, &request.session_id)
                    .expect("failed close returns its original owner to retryable state")
            ),
        }
    }
    let failed_pane = &pane_ids[1];
    assert_eq!(runtime.session_for_pane(failed_pane), Some(&failed_session));
    assert_eq!(
        runtime.pane(failed_pane).map(|pane| pane.phase),
        Some(RuntimePanePhase::CloseFailed)
    );
    controller
        .complete_resource_operation(&operation_id, Err(WorkspaceRestoreFailure::ResourceFailure))
        .expect("partial close acknowledgement leaves reducer operation retryable");
    let retry = controller
        .retry_pending_resource_operation()
        .expect("failed close remains available for explicit retry");
    assert_eq!(
        retry, operation,
        "retry preserves the same operation and pane IDs"
    );

    let retry_pane_ids = match &retry {
        WorkspaceResourceOperation::CloseTab { pane_ids, .. } => pane_ids,
        _ => panic!("retry retains the original CloseTab operation"),
    };
    let mut retry_requests = Vec::new();
    for pane_id in retry_pane_ids {
        if let Some(pane) = runtime.pane(pane_id) {
            let session_id = pane
                .session_id
                .clone()
                .expect("the failed owner still has its original session");
            assert_eq!(runtime.begin_close(pane_id), Ok(session_id.clone()));
            retry_requests.push(close_request(&tab_id, pane_id, session_id));
        }
    }
    assert_eq!(
        retry_requests.len(),
        1,
        "already-closed sessions are not recreated"
    );
    let retry_deadline = first_deadline + Duration::from_secs(1);
    assert_ne!(retry_deadline, first_deadline);
    let retry_results = block_on(close_sessions_until(
        retry_requests,
        retry_deadline,
        |_, _| async { Ok(()) },
    ));
    assert_eq!(retry_results.len(), 1);
    for (request, result) in retry_results {
        result.expect("retry receives a successful close acknowledgement");
        let WorkspaceCloseResource::Pane(pane_id) = request.resource else {
            panic!("fixture contains pane resources only");
        };
        assert!(
            runtime
                .remove_closed_session(&pane_id, &request.session_id)
                .expect("retry acknowledgement removes the retained owner")
        );
    }
    assert_eq!(runtime.panes().count(), 0);
    controller
        .complete_resource_operation(&operation_id, Ok(()))
        .expect("the same operation commits after every close acknowledgement");
    assert!(controller.snapshot().terminal_tabs.is_empty());
    assert!(controller.pending_resource_operation().is_none());
}
