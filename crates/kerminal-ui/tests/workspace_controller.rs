//! 工作区 reducer 覆盖恢复投影、资源提交门禁与 Missing/CAS 保存闭环。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_application::workspace::{
    WorkspaceFileAccess, WorkspaceFileSource, WorkspaceLocalMachineScope, WorkspaceMachineStatus,
    WorkspacePaneMode, WorkspaceSessionDocument, WorkspaceSessionExpectedRevision,
    WorkspaceSessionLoad, WorkspaceSessionPane, WorkspaceSessionRevision,
    WorkspaceSessionSaveOutcome, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTabGroup,
    WorkspaceTabGroupColor, WorkspaceTabGroupId, WorkspaceTargetRef,
    WorkspaceTargetUnavailableReason,
};
use kerminal_domain::{
    identity::{HostId, PaneId, TabId},
    workspace::{SplitDirection, TerminalLayoutNode},
};
use kerminal_ui::features::{
    workspace_controller::{
        WorkspaceController, WorkspaceControllerEffect, WorkspacePersistenceState,
        WorkspaceResourceOperation, WorkspaceRestoreFailure, WorkspaceRestoreItem,
        WorkspaceRestoreItemId, WorkspaceRestoreStatus,
    },
    workspace_panes::WorkspacePaneEvent,
    workspace_tabs::{
        GroupTabMoveDirection, TabDropPlacement, WorkspaceCloseBatchIntent, WorkspaceTabEvent,
        WorkspaceTabKind,
    },
};

/// 测试使用固定的不透明业务 ID，避免随机值让失败用例难以定位。
fn pane_id(raw: &str) -> PaneId {
    PaneId::parse(raw).expect("valid pane ID")
}

/// 测试使用固定的不透明业务 ID，验证旧 v3 ID 不要求 UUID 格式。
fn tab_id(raw: &str) -> TabId {
    TabId::parse(raw).expect("valid tab ID")
}

/// 组装本地 pane 的完整领域 DTO，目标显式化以便恢复状态机安全判断。
fn local_pane(id: &str) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id: pane_id(id),
        title: id.to_owned(),
        machine_id: "machine-local".to_owned(),
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
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
        output_history: None,
    }
}

/// 构造单 pane terminal tab，所有测试都共享同一组 pane/tab 身份约束。
fn terminal_tab(id: &str, title: &str, pane: &PaneId) -> WorkspaceSessionTab {
    WorkspaceSessionTab::Terminal {
        id: tab_id(id),
        tab_group_id: None,
        title: title.to_owned(),
        machine_id: "machine-local".to_owned(),
        layout: TerminalLayoutNode::pane(pane.clone()),
    }
}

/// 生成有效快照，避免测试通过手写 JSON 绕过 typed codec 的约束。
fn one_terminal_snapshot() -> WorkspaceSessionSnapshot {
    let pane = local_pane("pane-a");
    WorkspaceSessionSnapshot {
        active_tab_id: Some(tab_id("tab-a")),
        focused_pane_id: Some(pane.id.clone()),
        selected_machine_id: Some("machine-local".to_owned()),
        terminal_tabs: vec![terminal_tab("tab-a", "Terminal", &pane.id)],
        terminal_panes: vec![pane],
        tab_groups: BTreeMap::new(),
    }
}

/// 构造完整 UI 中含 transient pane 的分屏及其可持久化投影，供 CAS 边界测试复用。
fn mixed_split_snapshots() -> (WorkspaceSessionSnapshot, WorkspaceSessionSnapshot) {
    let saved_host = HostId::new();
    let external_host = HostId::parse("external:temporary-target").expect("valid external ID");
    let mut saved_pane = local_pane("pane-a");
    saved_pane.machine_id = saved_host.as_str().to_owned();
    saved_pane.mode = WorkspacePaneMode::Ssh;
    saved_pane.target = Some(WorkspaceTargetRef::Ssh {
        host_id: saved_host.clone(),
    });
    saved_pane.remote_host_id = Some(saved_host.clone());
    saved_pane.local_machine_scope = None;

    let mut external_pane = local_pane("pane-external");
    external_pane.machine_id = external_host.as_str().to_owned();
    external_pane.mode = WorkspacePaneMode::Ssh;
    external_pane.target = Some(WorkspaceTargetRef::Ssh {
        host_id: external_host.clone(),
    });
    external_pane.remote_host_id = Some(external_host.clone());
    external_pane.local_machine_scope = None;

    let layout = TerminalLayoutNode::pane(saved_pane.id.clone())
        .split_pane(
            &saved_pane.id,
            external_pane.id.clone(),
            SplitDirection::Horizontal,
            "split-external",
            false,
        )
        .expect("valid split layout");
    let full = WorkspaceSessionSnapshot {
        active_tab_id: Some(tab_id("tab-a")),
        focused_pane_id: Some(external_pane.id.clone()),
        selected_machine_id: Some(external_host.as_str().to_owned()),
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id("tab-a"),
            tab_group_id: None,
            title: "Mixed split".to_owned(),
            machine_id: saved_host.as_str().to_owned(),
            layout,
        }],
        terminal_panes: vec![saved_pane.clone(), external_pane.clone()],
        tab_groups: BTreeMap::new(),
    };
    let mut projected = full.clone();
    let WorkspaceSessionTab::Terminal { layout, .. } = &mut projected.terminal_tabs[0] else {
        unreachable!("fixture contains a terminal tab");
    };
    *layout = layout
        .remove_pane(&external_pane.id)
        .expect("transient pane can be removed from split")
        .expect("saved pane remains in the split");
    projected
        .terminal_panes
        .retain(|pane| pane.id != external_pane.id);
    projected.focused_pane_id = Some(saved_pane.id.clone());
    projected.selected_machine_id = Some(saved_host.as_str().to_owned());
    full.validate().expect("full fixture is valid");
    projected.validate().expect("projection fixture is valid");
    (full, projected)
}

/// 构造不含终端资源的 transfer tab，便于在保存期间触发有效的 persistent 选择变化。
fn transfer_tab() -> WorkspaceSessionTab {
    transfer_tab_with("tab-transfer", "Transfer", None)
}

/// 构造可指定身份和分组的非终端标签，批量关闭测试不需要伪造 pane 资源。
fn transfer_tab_with(id: &str, title: &str, group_id: Option<&str>) -> WorkspaceSessionTab {
    WorkspaceSessionTab::SftpTransfer {
        id: tab_id(id),
        tab_group_id: group_id
            .map(WorkspaceTabGroupId::parse)
            .transpose()
            .unwrap(),
        title: title.to_owned(),
        machine_id: "machine-remote".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: None,
        initial_right_selection: None,
    }
}

/// 构造一组有序标签与空组，保持终端 pane 单一 owner 以验证只变更标签 reducer。
fn grouped_tabs_snapshot() -> WorkspaceSessionSnapshot {
    let mut snapshot = one_terminal_snapshot();
    let group_id = WorkspaceTabGroupId::parse("group-a").unwrap();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut snapshot.terminal_tabs[0] {
        *tab_group_id = Some(group_id.clone());
    }
    snapshot.terminal_tabs.extend([
        transfer_tab_with("tab-b", "B", Some("group-a")),
        transfer_tab_with("tab-c", "C", Some("group-a")),
        transfer_tab_with("tab-d", "D", None),
    ]);
    snapshot.tab_groups.insert(
        group_id,
        WorkspaceTabGroup {
            title: "Group A".to_owned(),
            color: Some(WorkspaceTabGroupColor::Blue),
            collapsed: false,
        },
    );
    snapshot
        .validate()
        .expect("grouped workspace snapshot is valid");
    snapshot
}

/// 模拟 Root 在另一个终端创建成功后同步完整快照，保留明确目标和新的稳定 pane ID。
fn snapshot_with_second_local_terminal(
    snapshot: &WorkspaceSessionSnapshot,
) -> WorkspaceSessionSnapshot {
    let mut next = snapshot.clone();
    let pane = local_pane("pane-b");
    next.terminal_panes.push(pane.clone());
    next.terminal_tabs
        .push(terminal_tab("tab-b", "Terminal B", &pane.id));
    next.active_tab_id = Some(tab_id("tab-b"));
    next.focused_pane_id = Some(pane.id);
    next.selected_machine_id = Some("machine-local".to_owned());
    next.validate().expect("second terminal snapshot is valid");
    next
}

/// 模拟保存期间第二个外部 pane 创建完成；它必须留在完整 UI 快照且不进入 durable projection。
fn snapshot_with_second_transient_pane(
    snapshot: &WorkspaceSessionSnapshot,
) -> WorkspaceSessionSnapshot {
    let mut next = snapshot.clone();
    let external_host = HostId::parse("external:temporary-target-b").expect("valid external ID");
    let mut pane = local_pane("pane-external-b");
    pane.machine_id = external_host.as_str().to_owned();
    pane.mode = WorkspacePaneMode::Ssh;
    pane.target = Some(WorkspaceTargetRef::Ssh {
        host_id: external_host.clone(),
    });
    pane.remote_host_id = Some(external_host.clone());
    pane.local_machine_scope = None;

    let WorkspaceSessionTab::Terminal { layout, .. } = &mut next.terminal_tabs[0] else {
        unreachable!("fixture contains a terminal tab");
    };
    *layout = layout
        .split_pane(
            &pane_id("pane-external"),
            pane.id.clone(),
            SplitDirection::Vertical,
            "split-external-b",
            false,
        )
        .expect("new transient pane can be split into the terminal layout");
    next.terminal_panes.push(pane.clone());
    next.focused_pane_id = Some(pane.id);
    next.selected_machine_id = Some(external_host.as_str().to_owned());
    next.validate()
        .expect("second transient pane snapshot is valid");
    next
}

/// 从 typed 快照生成可读文档，revision 始终由调用方作为独立 CAS 输入。
fn document_for(snapshot: &WorkspaceSessionSnapshot) -> WorkspaceSessionDocument {
    let mut document = WorkspaceSessionDocument::empty_v3();
    document
        .update_known_fields(snapshot)
        .expect("typed snapshot should encode as v3");
    document
}

/// 从缺失 session 首次创建资源后仍以 Missing CAS 保存，绝不虚构旧 revision。
#[test]
fn missing_baseline_synchronizes_created_tab_and_keeps_no_clobber_cas() {
    let mut controller = WorkspaceController::from_missing().unwrap();
    let snapshot = one_terminal_snapshot();
    let mut failed_candidate = snapshot.clone();
    failed_candidate.terminal_panes[0].target = None;

    assert!(controller.synchronize_snapshot(failed_candidate).is_err());
    assert!(!controller.has_unsaved_or_pending_work());
    assert!(controller.snapshot().terminal_tabs.is_empty());

    let effect = controller.synchronize_snapshot(snapshot.clone()).unwrap();
    assert!(matches!(effect, WorkspaceControllerEffect::Changed { .. }));
    assert_eq!(controller.snapshot(), &snapshot);
    assert!(matches!(
        controller.restore_status(&WorkspaceRestoreItemId::Pane(pane_id("pane-a"))),
        Some(WorkspaceRestoreStatus::Ready)
    ));

    let request = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        request.expected_revision,
        WorkspaceSessionExpectedRevision::Missing
    );
    assert_eq!(
        request.candidate_document.workspace_snapshot().unwrap(),
        snapshot
    );
}

/// transient pane 仍保留在完整工作区；CAS 只提交过滤后的分屏且回执后状态 clean。
#[test]
fn projected_save_keeps_transient_split_visible_and_clean() {
    let (full, projected) = mixed_split_snapshots();
    let mut controller = WorkspaceController::from_missing().unwrap();
    controller.synchronize_snapshot(full.clone()).unwrap();

    let request = controller
        .begin_save_projected(projected.clone())
        .unwrap()
        .unwrap();
    assert_eq!(request.source_snapshot, full);
    assert_eq!(request.desired_snapshot, projected);
    let saved = request.candidate_document.workspace_snapshot().unwrap();
    assert_eq!(saved.terminal_panes.len(), 1);
    assert_eq!(saved.terminal_panes[0].id, pane_id("pane-a"));
    assert!(matches!(
        saved.terminal_tabs[0].terminal_layout(),
        Some(TerminalLayoutNode::Pane {
            pane_id: actual_pane_id,
        }) if actual_pane_id == &pane_id("pane-a")
    ));

    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("revision-projected"),
            },
        )
        .unwrap();
    assert_eq!(controller.snapshot(), &full);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Clean
    ));
    assert!(!controller.has_unsaved_or_pending_work());
}

/// transient-only 变化经投影与持久化基线相等时无 I/O，不移除界面里的运行资源。
#[test]
fn transient_only_projection_cleans_working_state_without_save_request() {
    let (full, projected) = mixed_split_snapshots();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&projected),
        WorkspaceSessionRevision::new("revision-base"),
    )
    .unwrap();
    controller.synchronize_snapshot(full.clone()).unwrap();
    assert!(controller.has_unsaved_or_pending_work());

    assert!(
        controller
            .begin_save_projected(projected)
            .unwrap()
            .is_none()
    );
    assert_eq!(controller.snapshot(), &full);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Clean
    ));
    assert!(!controller.has_unsaved_or_pending_work());
}

/// 保存期间发生新的 persistent 选择后，旧回执只确认旧请求快照并保留 dirty 状态。
#[test]
fn projected_save_receipt_does_not_clean_changes_made_while_saving() {
    let base = one_terminal_snapshot();
    let mut working = base.clone();
    working.terminal_tabs.push(transfer_tab());
    let mut controller = WorkspaceController::from_loaded(
        document_for(&base),
        WorkspaceSessionRevision::new("revision-base"),
    )
    .unwrap();
    controller.synchronize_snapshot(working.clone()).unwrap();
    let request = controller
        .begin_save_projected(working.clone())
        .unwrap()
        .unwrap();

    controller
        .apply_tab_event(WorkspaceTabEvent::Activate(tab_id("tab-transfer")))
        .unwrap();
    let newer_working = controller.snapshot().clone();
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("revision-saved"),
            },
        )
        .unwrap();

    assert_eq!(controller.snapshot(), &newer_working);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Dirty
    ));
    assert!(controller.has_unsaved_or_pending_work());
}

/// 保存 A 期间允许 Root 同步资源 B；A 的旧回执只确认 A，B 仍可按新 revision 再保存。
#[test]
fn resource_ack_during_save_is_preserved_and_remains_dirty_until_next_cas() {
    let base = one_terminal_snapshot();
    let mut saving_snapshot = base.clone();
    saving_snapshot.terminal_tabs.push(transfer_tab());
    let mut controller = WorkspaceController::from_loaded(
        document_for(&base),
        WorkspaceSessionRevision::new("revision-base"),
    )
    .unwrap();
    controller
        .synchronize_snapshot(saving_snapshot.clone())
        .unwrap();
    let first_request = controller.begin_save().unwrap().unwrap();

    let acknowledged_snapshot = snapshot_with_second_local_terminal(&saving_snapshot);
    assert!(matches!(
        controller
            .synchronize_snapshot(acknowledged_snapshot.clone())
            .unwrap(),
        WorkspaceControllerEffect::Changed { .. }
    ));
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Saving { request_id, .. }
            if request_id == &first_request.request_id
    ));

    controller
        .finish_save(
            &first_request.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("revision-after-a"),
            },
        )
        .unwrap();

    assert_eq!(controller.snapshot(), &acknowledged_snapshot);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Dirty
    ));
    let second_request = controller.begin_save().unwrap().unwrap();
    assert_eq!(second_request.base_snapshot, saving_snapshot);
    assert_eq!(second_request.desired_snapshot, acknowledged_snapshot);
}

/// transient pane B 在投影保存 A 时到达；A 回执不丢资源，投影等于新基线后不重复写盘。
#[test]
fn transient_resource_ack_during_projected_save_stays_visible_without_extra_cas() {
    let (full_base, durable_base) = mixed_split_snapshots();
    let mut full_a = full_base.clone();
    full_a.terminal_tabs.push(transfer_tab());
    let mut durable_a = durable_base.clone();
    durable_a.terminal_tabs.push(transfer_tab());
    let mut controller = WorkspaceController::from_loaded(
        document_for(&durable_base),
        WorkspaceSessionRevision::new("revision-base"),
    )
    .unwrap();
    controller.synchronize_snapshot(full_a.clone()).unwrap();
    let request_a = controller
        .begin_save_projected(durable_a.clone())
        .unwrap()
        .unwrap();

    let full_b = snapshot_with_second_transient_pane(&full_a);
    controller.synchronize_snapshot(full_b.clone()).unwrap();
    controller
        .finish_save(
            &request_a.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("revision-after-a"),
            },
        )
        .unwrap();

    assert_eq!(controller.snapshot(), &full_b);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Dirty
    ));
    assert!(
        controller
            .begin_save_projected(durable_a)
            .unwrap()
            .is_none()
    );
    assert_eq!(controller.snapshot(), &full_b);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Clean
    ));
    assert!(!controller.has_unsaved_or_pending_work());
}

/// 未知提交按投影回读确认；UI 继续显示 transient pane，且其 HostId 不进入持久化基线。
#[test]
fn projected_unknown_readback_confirms_save_without_replacing_full_ui_snapshot() {
    let (full, projected) = mixed_split_snapshots();
    let mut controller = WorkspaceController::from_missing().unwrap();
    controller.synchronize_snapshot(full.clone()).unwrap();
    let request = controller
        .begin_save_projected(projected.clone())
        .unwrap()
        .unwrap();
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::OutcomeUnknown,
        )
        .unwrap();

    controller
        .resolve_unknown_readback(WorkspaceSessionLoad::Loaded {
            document: request.candidate_document.clone(),
            revision: WorkspaceSessionRevision::new("revision-readback"),
        })
        .unwrap();

    assert_eq!(controller.snapshot(), &full);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Clean
    ));
    assert!(
        controller
            .begin_save_projected(projected)
            .unwrap()
            .is_none()
    );
    assert_eq!(controller.snapshot(), &full);
}

/// 冲突覆盖必须重新提供安全投影，不能把当前 UI 快照中的 external HostId 写入候选文档。
#[test]
fn conflict_overwrite_persists_projection_without_external_identity() {
    let (full, projected) = mixed_split_snapshots();
    let mut controller = WorkspaceController::from_missing().unwrap();
    controller.synchronize_snapshot(full.clone()).unwrap();
    let initial_request = controller
        .begin_save_projected(projected.clone())
        .unwrap()
        .unwrap();
    controller
        .finish_save(
            &initial_request.request_id,
            WorkspaceSessionSaveOutcome::Conflict {
                latest: WorkspaceSessionLoad::Missing,
            },
        )
        .unwrap();
    controller.confirm_overwrite_latest().unwrap();
    assert_eq!(controller.snapshot(), &full);

    let overwrite = controller
        .begin_save_projected(projected.clone())
        .unwrap()
        .unwrap();
    assert_eq!(overwrite.desired_snapshot, projected);
    assert_eq!(
        overwrite.candidate_document.workspace_snapshot().unwrap(),
        overwrite.desired_snapshot
    );
    assert!(
        !overwrite
            .candidate_document
            .workspace_snapshot()
            .unwrap()
            .terminal_panes
            .iter()
            .any(|pane| pane
                .remote_host_id
                .as_ref()
                .is_some_and(|host| host.as_str().starts_with("external:")))
    );
    assert_eq!(controller.snapshot(), &full);
}

/// 运行态快照同步不能越过仍未确认的真实 session 创建/关闭操作。
#[test]
fn runtime_sync_is_rejected_while_resource_operation_is_pending() {
    let snapshot = one_terminal_snapshot();
    let document = document_for(&snapshot);
    let mut controller =
        WorkspaceController::from_loaded(document, WorkspaceSessionRevision::new("revision-a"))
            .unwrap();
    let event = WorkspacePaneEvent::Split {
        pane_id: pane_id("pane-a"),
        direction: SplitDirection::Horizontal,
        before: false,
    };
    assert!(matches!(
        controller
            .apply_pane_event(&tab_id("tab-a"), event)
            .unwrap(),
        WorkspaceControllerEffect::ResourceOperation(_)
    ));

    assert!(controller.synchronize_snapshot(snapshot).is_err());
    assert!(controller.pending_resource_operation().is_some());
}

/// 冲突后仍禁止 Root 同步快照覆盖并发决策，直到用户显式读回或确认覆盖。
#[test]
fn runtime_sync_remains_rejected_after_conflict() {
    let base = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&base),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let mut changed = base.clone();
    changed.terminal_tabs.push(transfer_tab());
    controller.synchronize_snapshot(changed.clone()).unwrap();
    let request = controller.begin_save().unwrap().unwrap();
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::Conflict {
                latest: WorkspaceSessionLoad::Missing,
            },
        )
        .unwrap();
    assert!(controller.synchronize_snapshot(base).is_err());
}

/// 保存冲突与异步分屏并发时，资源成功回执不得把冲突降级为普通 dirty 状态。
#[test]
fn resource_success_preserves_conflict_from_in_flight_save() {
    let base = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&base),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let mut changed = base.clone();
    changed.selected_machine_id = Some("machine-changed".to_owned());
    controller.synchronize_snapshot(changed).unwrap();
    let request = controller.begin_save().unwrap().unwrap();

    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_pane_event(
            &tab_id("tab-a"),
            WorkspacePaneEvent::Split {
                pane_id: pane_id("pane-a"),
                direction: SplitDirection::Vertical,
                before: false,
            },
        )
        .unwrap()
    else {
        panic!("split must request a real pane resource");
    };
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::Conflict {
                latest: WorkspaceSessionLoad::Missing,
            },
        )
        .unwrap();

    controller
        .complete_resource_operation(operation.operation_id(), Ok(()))
        .unwrap();

    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Conflict { .. }
    ));
    assert!(controller.begin_save().unwrap().is_none());
    assert_eq!(controller.snapshot().terminal_panes.len(), 2);
}

/// 分屏候选只在 Root 确认新 session 创建后提交，失败可重试或放弃且旧布局不变。
#[test]
fn split_commits_only_after_resource_success_and_failure_can_be_discarded() {
    let snapshot = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_pane_event(
            &tab_id("tab-a"),
            WorkspacePaneEvent::Split {
                pane_id: pane_id("pane-a"),
                direction: SplitDirection::Vertical,
                before: false,
            },
        )
        .unwrap()
    else {
        panic!("split must request a real pane resource");
    };
    let WorkspaceResourceOperation::CreatePane { pane, .. } = &operation else {
        panic!("split must create a pane");
    };
    assert_eq!(controller.snapshot().terminal_panes.len(), 1);
    assert_eq!(
        controller.snapshot().terminal_tabs[0]
            .terminal_layout()
            .unwrap()
            .pane_ids(),
        vec![pane_id("pane-a")]
    );

    controller
        .complete_resource_operation(
            operation.operation_id(),
            Err(WorkspaceRestoreFailure::ResourceFailure),
        )
        .unwrap();
    assert_eq!(
        controller.retry_pending_resource_operation(),
        Some(operation.clone())
    );
    assert!(controller.discard_pending_resource_operation());
    assert_eq!(controller.snapshot().terminal_panes.len(), 1);

    let WorkspaceControllerEffect::ResourceOperation(retry) = controller
        .apply_pane_event(
            &tab_id("tab-a"),
            WorkspacePaneEvent::Split {
                pane_id: pane_id("pane-a"),
                direction: SplitDirection::Vertical,
                before: false,
            },
        )
        .unwrap()
    else {
        panic!("a retried split must request a real pane resource");
    };
    let WorkspaceResourceOperation::CreatePane { pane: new_pane, .. } = &retry else {
        panic!("split must create a pane");
    };
    assert_ne!(new_pane.id, pane.id);
    let effect = controller
        .complete_resource_operation(retry.operation_id(), Ok(()))
        .unwrap();
    assert!(matches!(effect, WorkspaceControllerEffect::Changed { .. }));
    assert_eq!(controller.snapshot().terminal_panes.len(), 2);
    assert_eq!(
        controller.snapshot().terminal_tabs[0]
            .terminal_layout()
            .unwrap()
            .pane_ids()
            .len(),
        2
    );
    assert_eq!(
        controller.restore_status(&WorkspaceRestoreItemId::Pane(new_pane.id.clone())),
        Some(WorkspaceRestoreStatus::Ready)
    );
}

/// 指定主机分屏只生成独立 target pane，失败仍复用 CreatePane 的重试/放弃边界。
#[test]
fn explicit_host_split_uses_target_parameters_and_keeps_failure_recoverable() {
    let snapshot = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let host_id = HostId::new();

    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .begin_split_for_target(
            &tab_id("tab-a"),
            &pane_id("pane-a"),
            WorkspaceTargetRef::Ssh {
                host_id: host_id.clone(),
            },
            "Build host".to_owned(),
            SplitDirection::Horizontal,
            false,
        )
        .unwrap()
    else {
        panic!("explicit split must request a real pane resource");
    };
    let WorkspaceResourceOperation::CreatePane {
        pane,
        source_pane_id,
        ..
    } = &operation
    else {
        panic!("explicit split must use the existing CreatePane lifecycle");
    };
    assert_eq!(source_pane_id, &pane_id("pane-a"));
    assert_eq!(pane.title, "Build host");
    assert_eq!(pane.mode, WorkspacePaneMode::Ssh);
    assert_eq!(
        pane.target,
        Some(WorkspaceTargetRef::Ssh {
            host_id: host_id.clone()
        })
    );
    assert_eq!(pane.remote_host_id.as_ref(), Some(&host_id));
    assert_eq!(pane.machine_id, host_id.as_str());
    assert_eq!(pane.shell, None);
    assert_eq!(pane.cwd, None);
    assert_eq!(pane.output_history, None);
    assert_eq!(controller.snapshot().terminal_panes.len(), 1);
    assert_eq!(
        controller.snapshot().terminal_tabs[0]
            .terminal_layout()
            .unwrap()
            .pane_ids(),
        vec![pane_id("pane-a")]
    );

    controller
        .complete_resource_operation(
            operation.operation_id(),
            Err(WorkspaceRestoreFailure::ResourceFailure),
        )
        .unwrap();
    assert_eq!(
        controller.retry_pending_resource_operation(),
        Some(operation.clone())
    );
    assert!(controller.discard_pending_resource_operation());
    assert_eq!(controller.snapshot().terminal_panes.len(), 1);
}

/// 只有 picker 的显式确认才允许调用目标分屏用例；取消意图与无效来源均不预留资源。
#[test]
fn target_picker_request_and_stale_source_do_not_create_panes() {
    let snapshot = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    let cancel = controller.apply_pane_event(
        &tab_id("tab-a"),
        WorkspacePaneEvent::ChooseSplitTarget {
            pane_id: pane_id("pane-a"),
            direction: SplitDirection::Vertical,
            before: false,
        },
    );
    assert!(cancel.is_err());
    assert!(controller.pending_resource_operation().is_none());

    let stale_source = controller.begin_split_for_target(
        &tab_id("tab-a"),
        &pane_id("pane-missing"),
        WorkspaceTargetRef::Local {
            profile_id: Some("profile:dev".to_owned()),
        },
        "Dev profile".to_owned(),
        SplitDirection::Vertical,
        false,
    );
    assert!(stale_source.is_err());
    assert!(controller.pending_resource_operation().is_none());
    assert_eq!(controller.snapshot().terminal_panes.len(), 1);
}

/// 本地默认 shell 与不支持的历史目标不能进入“选择目标”分屏，避免静默回退到默认终端。
#[test]
fn target_split_rejects_default_local_and_unavailable_targets() {
    let snapshot = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let invalid_targets = [
        WorkspaceTargetRef::Local { profile_id: None },
        WorkspaceTargetRef::Unavailable {
            raw: serde_json::json!({"kind": "unknown"}),
            reason: WorkspaceTargetUnavailableReason::UnknownKind,
        },
    ];

    for target in invalid_targets {
        assert!(
            controller
                .begin_split_for_target(
                    &tab_id("tab-a"),
                    &pane_id("pane-a"),
                    target,
                    "Invalid".to_owned(),
                    SplitDirection::Vertical,
                    false,
                )
                .is_err()
        );
        assert!(controller.pending_resource_operation().is_none());
    }
    assert_eq!(controller.snapshot().terminal_panes.len(), 1);
}

/// 恢复计划保留多级 pane、transfer、workspace file 与 group，不给非终端 tab 造 layout。
#[test]
fn restore_plan_and_projection_preserve_all_supported_tab_kinds() {
    let pane_a = local_pane("pane-a");
    let pane_b = local_pane("pane-b");
    let pane_c = local_pane("pane-c");
    let layout = TerminalLayoutNode::pane(pane_a.id.clone())
        .split_pane(
            &pane_a.id,
            pane_b.id.clone(),
            SplitDirection::Horizontal,
            "split-a",
            false,
        )
        .unwrap()
        .split_pane(
            &pane_b.id,
            pane_c.id.clone(),
            SplitDirection::Vertical,
            "split-b",
            true,
        )
        .unwrap();
    let group_id = WorkspaceTabGroupId::parse("group-a").unwrap();
    let terminal = WorkspaceSessionTab::Terminal {
        id: tab_id("tab-terminal"),
        tab_group_id: Some(group_id.clone()),
        title: "Terminal".to_owned(),
        machine_id: "machine-local".to_owned(),
        layout,
    };
    let transfer = WorkspaceSessionTab::SftpTransfer {
        id: tab_id("tab-transfer"),
        tab_group_id: Some(group_id.clone()),
        title: "Transfer".to_owned(),
        machine_id: "machine-local".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: Some("/tmp".to_owned()),
        initial_right_selection: None,
    };
    let file = WorkspaceSessionTab::WorkspaceFile {
        id: tab_id("tab-file"),
        tab_group_id: None,
        title: "settings.toml".to_owned(),
        machine_id: "machine-local".to_owned(),
        target: WorkspaceTargetRef::Local { profile_id: None },
        path: "C:/tmp/settings.toml".to_owned(),
        root_path: Some("C:/tmp".to_owned()),
        access: WorkspaceFileAccess::Editable,
        source: WorkspaceFileSource::Local,
    };
    let snapshot = WorkspaceSessionSnapshot {
        active_tab_id: Some(tab_id("tab-terminal")),
        focused_pane_id: Some(pane_a.id.clone()),
        selected_machine_id: Some("machine-local".to_owned()),
        terminal_tabs: vec![terminal, transfer, file],
        terminal_panes: vec![pane_a.clone(), pane_b, pane_c],
        tab_groups: BTreeMap::from([(
            group_id,
            WorkspaceTabGroup {
                title: "Local".to_owned(),
                color: Some(WorkspaceTabGroupColor::Blue),
                collapsed: false,
            },
        )]),
    };
    let controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    let restore_plan = controller.restore_plan();
    assert_eq!(restore_plan.len(), 5);
    assert!(matches!(
        restore_plan[0],
        WorkspaceRestoreItem::TerminalPane { .. }
    ));
    assert!(matches!(
        restore_plan[3],
        WorkspaceRestoreItem::SftpTransfer { .. }
    ));
    assert!(matches!(
        restore_plan[4],
        WorkspaceRestoreItem::WorkspaceFile { .. }
    ));
    let projections = controller.tab_projections();
    assert_eq!(projections.len(), 3);
    assert!(matches!(&projections[0].kind, WorkspaceTabKind::Terminal));
    assert!(projections[0].layout.is_some());
    assert!(matches!(
        &projections[1].kind,
        WorkspaceTabKind::SftpTransfer
    ));
    assert!(projections[1].layout.is_none());
    assert!(matches!(
        &projections[2].kind,
        WorkspaceTabKind::WorkspaceFile
    ));
    assert!(projections[2].layout.is_none());
}

/// Root 的真实回执被错误 pane/tab ID 关联时拒绝，不能提交过期布局候选。
#[test]
fn stale_resource_receipt_cannot_commit_split_candidate() {
    let snapshot = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_pane_event(
            &tab_id("tab-a"),
            WorkspacePaneEvent::Split {
                pane_id: pane_id("pane-a"),
                direction: SplitDirection::Horizontal,
                before: true,
            },
        )
        .unwrap()
    else {
        panic!("split must request a pane resource");
    };

    assert!(
        controller
            .complete_resource_operation("stale-operation", Ok(()))
            .is_err()
    );
    assert_eq!(controller.snapshot().terminal_panes.len(), 1);
    assert_eq!(
        controller
            .pending_resource_operation()
            .map(|pending| pending.operation_id()),
        Some(operation.operation_id())
    );
}

/// 标签切换更新活跃 pane/目标并写入 dirty 状态，不重建 Root 的 session。
#[test]
fn tab_activation_updates_typed_selection_and_save_candidate() {
    let mut snapshot = one_terminal_snapshot();
    let transfer = WorkspaceSessionTab::SftpTransfer {
        id: tab_id("tab-transfer"),
        tab_group_id: None,
        title: "Transfer".to_owned(),
        machine_id: "machine-remote".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: None,
        initial_right_selection: None,
    };
    snapshot.terminal_tabs.push(transfer);
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::Activate(tab_id("tab-transfer")))
            .unwrap(),
        WorkspaceControllerEffect::Changed { .. }
    ));
    assert_eq!(
        controller.snapshot().active_tab_id,
        Some(tab_id("tab-transfer"))
    );
    assert_eq!(controller.snapshot().focused_pane_id, None);
    assert_eq!(
        controller.snapshot().selected_machine_id.as_deref(),
        Some("machine-remote")
    );
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Dirty
    ));
    let request = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        request.expected_revision,
        WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new("revision-a"))
    );
}

/// 标签重排和组迁移只修改 typed 快照，数组索引与 v3 稳定 group ID 保持一致。
#[test]
fn tab_reorder_and_group_move_are_persistable_typed_changes() {
    let mut snapshot = one_terminal_snapshot();
    let group_id = WorkspaceTabGroupId::parse("group-a").unwrap();
    snapshot.tab_groups.insert(
        group_id,
        WorkspaceTabGroup {
            title: "Pinned".to_owned(),
            color: Some(WorkspaceTabGroupColor::Mint),
            collapsed: false,
        },
    );
    snapshot
        .terminal_tabs
        .push(WorkspaceSessionTab::SftpTransfer {
            id: tab_id("tab-transfer"),
            tab_group_id: None,
            title: "Transfer".to_owned(),
            machine_id: "machine-remote".to_owned(),
            left_host_id: None,
            locked_left_host_id: None,
            right_host_id: None,
            external_launch_id: None,
            initial_left_path: None,
            initial_right_path: None,
            initial_right_selection: None,
        });
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    controller
        .apply_tab_event(WorkspaceTabEvent::MoveToGroup {
            tab_id: tab_id("tab-a"),
            group_id: Some("group-a".to_owned()),
        })
        .unwrap();
    controller
        .apply_tab_event(WorkspaceTabEvent::Reorder {
            tab_id: tab_id("tab-a"),
            target_index: 1,
        })
        .unwrap();

    assert_eq!(
        controller.snapshot().terminal_tabs[0].id(),
        &tab_id("tab-transfer")
    );
    assert_eq!(
        controller.snapshot().terminal_tabs[1].id(),
        &tab_id("tab-a")
    );
    assert_eq!(
        controller.snapshot().terminal_tabs[1]
            .tab_group_id()
            .map(WorkspaceTabGroupId::as_str),
        Some("group-a")
    );
    let request = controller.begin_save().unwrap().unwrap();
    let saved = request.candidate_document.workspace_snapshot().unwrap();
    assert_eq!(saved.terminal_tabs[0].id(), &tab_id("tab-transfer"));
    assert_eq!(saved.terminal_tabs[1].id(), &tab_id("tab-a"));
    let expected_group = WorkspaceTabGroupId::parse("group-a").unwrap();
    assert_eq!(saved.terminal_tabs[1].tab_group_id(), Some(&expected_group));
}

/// 前后落点对源标签位于目标两侧都使用移除后的目标位置，避免拖动方向改变落点语义。
///
/// @author kongweiguang
#[test]
fn explicit_tab_drop_placement_is_stable_when_source_is_before_or_after_target() {
    let mut snapshot = one_terminal_snapshot();
    snapshot.terminal_tabs.extend([
        transfer_tab_with("tab-b", "B", None),
        transfer_tab_with("tab-c", "C", None),
        transfer_tab_with("tab-d", "D", None),
    ]);
    snapshot.validate().expect("drop fixture is valid");
    let original_panes = snapshot.terminal_panes.clone();
    let original_active_tab_id = snapshot.active_tab_id.clone();
    let original_focused_pane_id = snapshot.focused_pane_id.clone();
    let original_selected_machine_id = snapshot.selected_machine_id.clone();

    for (source, target, placement, expected_order) in [
        (
            "tab-a",
            "tab-c",
            TabDropPlacement::Before,
            ["tab-b", "tab-a", "tab-c", "tab-d"],
        ),
        (
            "tab-a",
            "tab-c",
            TabDropPlacement::After,
            ["tab-b", "tab-c", "tab-a", "tab-d"],
        ),
        (
            "tab-c",
            "tab-a",
            TabDropPlacement::Before,
            ["tab-c", "tab-a", "tab-b", "tab-d"],
        ),
        (
            "tab-c",
            "tab-a",
            TabDropPlacement::After,
            ["tab-a", "tab-c", "tab-b", "tab-d"],
        ),
    ] {
        let mut controller = WorkspaceController::from_loaded(
            document_for(&snapshot),
            WorkspaceSessionRevision::new("revision-a"),
        )
        .unwrap();

        controller
            .apply_tab_event(WorkspaceTabEvent::DropOnTab {
                tab_id: tab_id(source),
                target_tab_id: tab_id(target),
                placement,
            })
            .unwrap();

        assert_eq!(
            controller
                .snapshot()
                .terminal_tabs
                .iter()
                .map(|tab| tab.id().as_str())
                .collect::<Vec<_>>(),
            expected_order
        );
        assert_eq!(controller.snapshot().terminal_panes, original_panes);
        assert_eq!(controller.snapshot().active_tab_id, original_active_tab_id);
        assert_eq!(
            controller.snapshot().focused_pane_id,
            original_focused_pane_id
        );
        assert_eq!(
            controller.snapshot().selected_machine_id,
            original_selected_machine_id
        );
    }
}

/// 显式前后落点在跨组时以稳定目标 ID 定位，迁移目标组归属且保持活动选择与 pane 身份。
///
/// @author kongweiguang
#[test]
fn cross_group_drop_honors_placement_and_preserves_session_identity() {
    let mut snapshot = one_terminal_snapshot();
    let group_a = WorkspaceTabGroupId::parse("group-a").unwrap();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut snapshot.terminal_tabs[0] {
        *tab_group_id = Some(group_a.clone());
    }
    snapshot.terminal_tabs.extend([
        transfer_tab_with("tab-b", "B", Some("group-a")),
        transfer_tab_with("tab-c", "C", Some("group-b")),
        transfer_tab_with("tab-d", "D", Some("group-b")),
        transfer_tab_with("tab-u", "Ungrouped", None),
    ]);
    snapshot.tab_groups.insert(
        group_a.clone(),
        WorkspaceTabGroup {
            title: "A".to_owned(),
            color: Some(WorkspaceTabGroupColor::Blue),
            collapsed: false,
        },
    );
    snapshot.tab_groups.insert(
        WorkspaceTabGroupId::parse("group-b").unwrap(),
        WorkspaceTabGroup {
            title: "B".to_owned(),
            color: Some(WorkspaceTabGroupColor::Mint),
            collapsed: true,
        },
    );
    snapshot.validate().expect("drop fixture is valid");
    let original_panes = snapshot.terminal_panes.clone();
    let original_active_tab_id = snapshot.active_tab_id.clone();
    let original_focused_pane_id = snapshot.focused_pane_id.clone();
    let original_selected_machine_id = snapshot.selected_machine_id.clone();
    let (original_terminal_machine_id, original_terminal_layout) = match &snapshot.terminal_tabs[0]
    {
        WorkspaceSessionTab::Terminal {
            machine_id, layout, ..
        } => (machine_id.clone(), layout.clone()),
        _ => unreachable!("tab-a is the terminal fixture"),
    };

    for (placement, expected_order) in [
        (
            TabDropPlacement::Before,
            ["tab-b", "tab-a", "tab-c", "tab-d", "tab-u"],
        ),
        (
            TabDropPlacement::After,
            ["tab-b", "tab-c", "tab-a", "tab-d", "tab-u"],
        ),
    ] {
        let mut controller = WorkspaceController::from_loaded(
            document_for(&snapshot),
            WorkspaceSessionRevision::new("revision-a"),
        )
        .unwrap();

        assert!(matches!(
            controller
                .apply_tab_event(WorkspaceTabEvent::DropOnTab {
                    tab_id: tab_id("tab-a"),
                    target_tab_id: tab_id("tab-c"),
                    placement,
                })
                .unwrap(),
            WorkspaceControllerEffect::Changed { .. }
        ));

        let updated = controller.snapshot();
        assert_eq!(
            updated
                .terminal_tabs
                .iter()
                .map(|tab| tab.id().as_str())
                .collect::<Vec<_>>(),
            expected_order
        );
        assert_eq!(
            updated
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == &tab_id("tab-a"))
                .and_then(WorkspaceSessionTab::tab_group_id)
                .map(WorkspaceTabGroupId::as_str),
            Some("group-b")
        );
        let moved_terminal = updated
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == &tab_id("tab-a"))
            .expect("moved terminal remains present");
        let WorkspaceSessionTab::Terminal {
            machine_id, layout, ..
        } = moved_terminal
        else {
            unreachable!("drop preserves the tab kind")
        };
        assert_eq!(machine_id, &original_terminal_machine_id);
        assert_eq!(layout, &original_terminal_layout);
        assert!(updated.tab_groups.contains_key(&group_a));
        assert_eq!(updated.terminal_panes, original_panes);
        assert_eq!(updated.active_tab_id, original_active_tab_id);
        assert_eq!(updated.focused_pane_id, original_focused_pane_id);
        assert_eq!(updated.selected_machine_id, original_selected_machine_id);
    }
}

/// 拖入未分组标签会退出源组；最后成员离组时一并清理已无用户可见入口的组定义。
///
/// @author kongweiguang
#[test]
fn drop_on_ungrouped_tab_cleans_the_empty_source_group() {
    let mut snapshot = one_terminal_snapshot();
    let group_id = WorkspaceTabGroupId::parse("group-a").unwrap();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut snapshot.terminal_tabs[0] {
        *tab_group_id = Some(group_id.clone());
    }
    snapshot
        .terminal_tabs
        .push(transfer_tab_with("tab-u", "Ungrouped", None));
    snapshot.tab_groups.insert(
        group_id.clone(),
        WorkspaceTabGroup {
            title: "Last member".to_owned(),
            color: Some(WorkspaceTabGroupColor::Amber),
            collapsed: true,
        },
    );
    snapshot.validate().expect("drop fixture is valid");
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    controller
        .apply_tab_event(WorkspaceTabEvent::DropOnTab {
            tab_id: tab_id("tab-a"),
            target_tab_id: tab_id("tab-u"),
            placement: TabDropPlacement::After,
        })
        .unwrap();

    assert_eq!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-u", "tab-a"]
    );
    assert!(controller.snapshot().tab_groups.is_empty());
    assert!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .all(|tab| tab.tab_group_id().is_none())
    );
}

/// 尾部空白把组内终端一次性移到未分组末尾，并只产生一代快照变更与一个 CAS 请求。
/// 身份对照只清除预期改变的组引用，继续逐字段核对其余终端数据没有被重建或改绑。
///
/// @author kongweiguang
#[test]
fn trailing_drop_ungroups_and_appends_after_existing_ungrouped_tabs_once() {
    let mut snapshot = one_terminal_snapshot();
    let group_id = WorkspaceTabGroupId::parse("group-a").unwrap();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut snapshot.terminal_tabs[0] {
        *tab_group_id = Some(group_id.clone());
    }
    snapshot
        .terminal_tabs
        .push(transfer_tab_with("tab-b", "B", None));
    snapshot.tab_groups.insert(
        group_id.clone(),
        WorkspaceTabGroup {
            title: "A group".to_owned(),
            color: Some(WorkspaceTabGroupColor::Blue),
            collapsed: false,
        },
    );
    snapshot.validate().expect("tail-drop fixture is valid");
    let original_panes = snapshot.terminal_panes.clone();
    let original_active_tab_id = snapshot.active_tab_id.clone();
    let original_focused_pane_id = snapshot.focused_pane_id.clone();
    let original_selected_machine_id = snapshot.selected_machine_id.clone();
    let mut expected_terminal = snapshot.terminal_tabs[0].clone();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut expected_terminal {
        *tab_group_id = None;
    }
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    let effect = controller
        .apply_tab_event(WorkspaceTabEvent::MoveToUngroupedEnd(tab_id("tab-a")))
        .unwrap();
    let WorkspaceControllerEffect::Changed { generation } = effect else {
        panic!("tail drop must commit its combined reorder and ungrouping")
    };

    let updated = controller.snapshot();
    assert_eq!(
        updated
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-b", "tab-a"]
    );
    assert!(
        updated
            .terminal_tabs
            .iter()
            .all(|tab| tab.tab_group_id().is_none())
    );
    assert!(
        updated.tab_groups.is_empty(),
        "empty source group is removed"
    );
    assert_eq!(updated.terminal_tabs[1], expected_terminal);
    assert_eq!(updated.terminal_panes, original_panes);
    assert_eq!(updated.active_tab_id, original_active_tab_id);
    assert_eq!(updated.focused_pane_id, original_focused_pane_id);
    assert_eq!(updated.selected_machine_id, original_selected_machine_id);

    let save = controller
        .begin_save()
        .unwrap()
        .expect("the combined change produces one save request");
    assert_eq!(save.generation, generation);
    assert_eq!(
        save.candidate_document
            .workspace_snapshot()
            .unwrap()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-b", "tab-a"]
    );
    assert!(
        controller.begin_save().unwrap().is_none(),
        "an in-flight tail drop cannot emit a second CAS request"
    );
}

/// 没有既有未分组标签时仍以单个意图把源移到投影末尾并清理空源组。
///
/// @author kongweiguang
#[test]
fn trailing_drop_works_without_existing_ungrouped_target() {
    let mut snapshot = one_terminal_snapshot();
    let group_a = WorkspaceTabGroupId::parse("group-a").unwrap();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut snapshot.terminal_tabs[0] {
        *tab_group_id = Some(group_a.clone());
    }
    snapshot
        .terminal_tabs
        .push(transfer_tab_with("tab-b", "B", Some("group-b")));
    snapshot.tab_groups.insert(
        group_a.clone(),
        WorkspaceTabGroup {
            title: "A".to_owned(),
            color: Some(WorkspaceTabGroupColor::Blue),
            collapsed: false,
        },
    );
    let group_b = WorkspaceTabGroupId::parse("group-b").unwrap();
    snapshot.tab_groups.insert(
        group_b.clone(),
        WorkspaceTabGroup {
            title: "B".to_owned(),
            color: Some(WorkspaceTabGroupColor::Mint),
            collapsed: false,
        },
    );
    snapshot
        .validate()
        .expect("group-only tail-drop fixture is valid");
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::MoveToUngroupedEnd(tab_id("tab-a")))
            .unwrap(),
        WorkspaceControllerEffect::Changed { .. }
    ));
    assert_eq!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-b", "tab-a"]
    );
    assert!(
        controller.snapshot().terminal_tabs[1]
            .tab_group_id()
            .is_none()
    );
    assert!(!controller.snapshot().tab_groups.contains_key(&group_a));
    assert!(controller.snapshot().tab_groups.contains_key(&group_b));
}

/// 已在未分组末尾的标签不生成持久化变更，过期源则失败关闭且不碰快照。
///
/// @author kongweiguang
#[test]
fn trailing_drop_at_end_is_noop_and_stale_source_is_unchanged() {
    let mut snapshot = one_terminal_snapshot();
    let group_id = WorkspaceTabGroupId::parse("group-a").unwrap();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut snapshot.terminal_tabs[0] {
        *tab_group_id = Some(group_id.clone());
    }
    snapshot
        .terminal_tabs
        .push(transfer_tab_with("tab-b", "B", None));
    snapshot.tab_groups.insert(
        group_id,
        WorkspaceTabGroup {
            title: "A group".to_owned(),
            color: Some(WorkspaceTabGroupColor::Blue),
            collapsed: false,
        },
    );
    snapshot.validate().expect("ungrouped fixture is valid");
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let original = controller.snapshot().clone();

    assert_eq!(
        controller
            .apply_tab_event(WorkspaceTabEvent::MoveToUngroupedEnd(tab_id("tab-b")))
            .unwrap(),
        WorkspaceControllerEffect::Unchanged
    );
    assert_eq!(controller.snapshot(), &original);
    assert!(controller.begin_save().unwrap().is_none());

    assert!(
        controller
            .apply_tab_event(WorkspaceTabEvent::MoveToUngroupedEnd(tab_id("tab-stale")))
            .is_err()
    );
    assert_eq!(controller.snapshot(), &original);
    assert_eq!(
        controller.persistence_state(),
        &WorkspacePersistenceState::Clean
    );
    assert!(controller.begin_save().unwrap().is_none());
}

/// 同组拖放只按命中标签排序，不清除仍在使用的组定义或改变任何资源身份。
///
/// @author kongweiguang
#[test]
fn drop_within_same_group_reorders_without_changing_group_membership() {
    let mut snapshot = grouped_tabs_snapshot();
    snapshot.terminal_tabs[2] = transfer_tab_with("tab-c", "C", Some("group-a"));
    snapshot
        .validate()
        .expect("same-group drop fixture is valid");
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    controller
        .apply_tab_event(WorkspaceTabEvent::DropOnTab {
            tab_id: tab_id("tab-c"),
            target_tab_id: tab_id("tab-a"),
            placement: TabDropPlacement::Before,
        })
        .unwrap();

    assert_eq!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-c", "tab-a", "tab-b", "tab-d"]
    );
    assert!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .take(3)
            .all(|tab| tab
                .tab_group_id()
                .is_some_and(|id| id.as_str() == "group-a"))
    );
    assert!(
        controller
            .snapshot()
            .tab_groups
            .contains_key(&WorkspaceTabGroupId::parse("group-a").unwrap())
    );
}

/// 自拖放与源/目标失效均不改变快照，保证迟到 UI 事件不会篡改新工作区状态。
///
/// @author kongweiguang
#[test]
fn self_or_stale_tab_drop_leaves_snapshot_unchanged() {
    let snapshot = grouped_tabs_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let original = controller.snapshot().clone();

    assert_eq!(
        controller
            .apply_tab_event(WorkspaceTabEvent::DropOnTab {
                tab_id: tab_id("tab-a"),
                target_tab_id: tab_id("tab-a"),
                placement: TabDropPlacement::Before,
            })
            .unwrap(),
        WorkspaceControllerEffect::Unchanged
    );
    assert_eq!(controller.snapshot(), &original);
    assert!(
        controller
            .apply_tab_event(WorkspaceTabEvent::DropOnTab {
                tab_id: tab_id("tab-missing"),
                target_tab_id: tab_id("tab-a"),
                placement: TabDropPlacement::Before,
            })
            .is_err()
    );
    assert_eq!(controller.snapshot(), &original);
    assert!(
        controller
            .apply_tab_event(WorkspaceTabEvent::DropOnTab {
                tab_id: tab_id("tab-a"),
                target_tab_id: tab_id("tab-missing"),
                placement: TabDropPlacement::Before,
            })
            .is_err()
    );
    assert_eq!(controller.snapshot(), &original);
}

/// 新建命名组进入既有 v3 聚合和 CAS 候选；重复名称返回可由 Root 显示的业务错误。
#[test]
fn tab_group_creation_is_named_and_persistable() {
    let snapshot = one_terminal_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::CreateGroupForTab {
                tab_id: tab_id("tab-a"),
                title: "  远程主机  ".to_owned(),
            })
            .unwrap(),
        WorkspaceControllerEffect::Changed { .. }
    ));
    let (group_id, group) = controller.snapshot().tab_groups.iter().next().unwrap();
    assert!(group_id.as_str().starts_with("group-"));
    assert_eq!(group.title, "远程主机");
    assert_eq!(group.color, None);
    assert!(!group.collapsed);

    let request = controller.begin_save().unwrap().unwrap();
    let saved = request.candidate_document.workspace_snapshot().unwrap();
    assert_eq!(saved.tab_groups, controller.snapshot().tab_groups);

    assert!(
        controller
            .apply_tab_event(WorkspaceTabEvent::CreateGroupForTab {
                tab_id: tab_id("tab-a"),
                title: "远程主机".to_owned(),
            })
            .is_err()
    );
}

/// 分组顺序在 v3 中由标签首次出现顺序表达，重排会规范数组并保留组内标签顺序。
#[test]
fn group_order_is_derived_from_tabs_and_reordering_is_persistable() {
    let mut snapshot = one_terminal_snapshot();
    let group_a = WorkspaceTabGroupId::parse("group-a").unwrap();
    let group_b = WorkspaceTabGroupId::parse("group-b").unwrap();
    if let WorkspaceSessionTab::Terminal { tab_group_id, .. } = &mut snapshot.terminal_tabs[0] {
        *tab_group_id = Some(group_b.clone());
    }
    snapshot.terminal_tabs.extend([
        transfer_tab_with("tab-b", "B", Some("group-a")),
        transfer_tab_with("tab-c", "C", Some("group-b")),
        transfer_tab_with("tab-d", "D", None),
    ]);
    snapshot.tab_groups.insert(
        group_a.clone(),
        WorkspaceTabGroup {
            title: "A".to_owned(),
            color: None,
            collapsed: false,
        },
    );
    snapshot.tab_groups.insert(
        group_b.clone(),
        WorkspaceTabGroup {
            title: "B".to_owned(),
            color: Some(WorkspaceTabGroupColor::Teal),
            collapsed: true,
        },
    );
    snapshot.tab_groups.insert(
        WorkspaceTabGroupId::parse("group-empty").unwrap(),
        WorkspaceTabGroup {
            title: "Empty".to_owned(),
            color: None,
            collapsed: false,
        },
    );
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    assert_eq!(
        controller
            .tab_projections()
            .iter()
            .map(|tab| tab.id.clone())
            .collect::<Vec<_>>(),
        [
            tab_id("tab-a"),
            tab_id("tab-c"),
            tab_id("tab-b"),
            tab_id("tab-d")
        ]
    );
    assert_eq!(
        controller
            .tab_group_projections()
            .iter()
            .map(|group| group.id.as_str())
            .collect::<Vec<_>>(),
        ["group-b", "group-a", "group-empty"]
    );

    controller
        .apply_tab_event(WorkspaceTabEvent::MoveGroup {
            group_id: "group-a".to_owned(),
            before_group_id: Some("group-b".to_owned()),
        })
        .unwrap();

    assert_eq!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().clone())
            .collect::<Vec<_>>(),
        [
            tab_id("tab-b"),
            tab_id("tab-a"),
            tab_id("tab-c"),
            tab_id("tab-d")
        ]
    );
    assert_eq!(
        controller
            .tab_group_projections()
            .iter()
            .map(|group| group.id.as_str())
            .collect::<Vec<_>>(),
        ["group-a", "group-b", "group-empty"]
    );
    assert_eq!(
        controller.snapshot().tab_groups[&group_b].color,
        Some(WorkspaceTabGroupColor::Teal)
    );
    assert!(controller.snapshot().tab_groups[&group_b].collapsed);
}

/// 标签改名、组元数据编辑和解组只改快照已知字段，不损失其他组属性或成员标签。
#[test]
fn tab_group_metadata_and_ungroup_are_complete_typed_operations() {
    let snapshot = grouped_tabs_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    controller
        .apply_tab_event(WorkspaceTabEvent::RenameTab {
            tab_id: tab_id("tab-b"),
            title: "Build host".to_owned(),
        })
        .unwrap();
    controller
        .apply_tab_event(WorkspaceTabEvent::RenameGroup {
            group_id: "group-a".to_owned(),
            title: "Production".to_owned(),
        })
        .unwrap();
    controller
        .apply_tab_event(WorkspaceTabEvent::SetGroupColor {
            group_id: "group-a".to_owned(),
            color: Some(WorkspaceTabGroupColor::Amber),
        })
        .unwrap();
    controller
        .apply_tab_event(WorkspaceTabEvent::SetGroupCollapsed {
            group_id: "group-a".to_owned(),
            collapsed: true,
        })
        .unwrap();

    let group_id = WorkspaceTabGroupId::parse("group-a").unwrap();
    let group = &controller.snapshot().tab_groups[&group_id];
    assert_eq!(group.title, "Production");
    assert_eq!(group.color, Some(WorkspaceTabGroupColor::Amber));
    assert!(group.collapsed);
    assert_eq!(controller.snapshot().terminal_tabs[1].title(), "Build host");

    controller
        .apply_tab_event(WorkspaceTabEvent::Ungroup("group-a".to_owned()))
        .unwrap();
    assert!(!controller.snapshot().tab_groups.contains_key(&group_id));
    assert!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .all(|tab| tab.tab_group_id().is_none())
    );
}

/// 批次失败停在当前标签；只接受原点击语义和剩余 ID，重试成功后再发起下一项。
#[test]
fn close_batch_stops_on_failure_and_retries_only_the_fixed_remainder() {
    let snapshot = grouped_tabs_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let original = WorkspaceCloseBatchIntent::Group {
        group_id: "group-a".to_owned(),
        tab_ids: vec![tab_id("tab-a"), tab_id("tab-b"), tab_id("tab-c")],
    };
    let WorkspaceControllerEffect::ResourceOperation(first) = controller
        .apply_tab_event(WorkspaceTabEvent::CloseBatch(original.clone()))
        .unwrap()
    else {
        panic!("closing a populated group must wait for tab resource acknowledgement");
    };
    assert!(matches!(
        &first,
        WorkspaceResourceOperation::CloseTab {
            tab_id: actual_tab_id,
            ..
        } if actual_tab_id == &tab_id("tab-a")
    ));
    controller
        .complete_resource_operation(first.operation_id(), Ok(()))
        .unwrap();
    let second = controller
        .advance_close_batch()
        .unwrap()
        .expect("the next fixed target must start only after the first close ACK");
    assert!(matches!(
        &second,
        WorkspaceResourceOperation::CloseTab {
            tab_id: actual_tab_id,
            ..
        } if actual_tab_id == &tab_id("tab-b")
    ));
    assert!(matches!(
        controller
            .complete_resource_operation(
                second.operation_id(),
                Err(WorkspaceRestoreFailure::ResourceFailure),
            )
            .unwrap(),
        WorkspaceControllerEffect::Unchanged
    ));
    assert_eq!(controller.pending_close_batch_remaining_count(), Some(2));
    assert_eq!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().clone())
            .collect::<Vec<_>>(),
        [tab_id("tab-b"), tab_id("tab-c"), tab_id("tab-d")]
    );

    assert!(controller.retry_pending_close_batch(&original).is_none());
    let remainder = WorkspaceCloseBatchIntent::Group {
        group_id: "group-a".to_owned(),
        tab_ids: vec![tab_id("tab-b"), tab_id("tab-c")],
    };
    assert_eq!(
        controller.retry_pending_close_batch(&remainder),
        Some(second.clone())
    );
    let expanded_remainder = WorkspaceCloseBatchIntent::Group {
        group_id: "group-a".to_owned(),
        tab_ids: vec![tab_id("tab-b"), tab_id("tab-c"), tab_id("tab-d")],
    };
    assert!(
        controller
            .retry_pending_close_batch(&expanded_remainder)
            .is_none()
    );
    assert!(
        controller
            .apply_tab_event(WorkspaceTabEvent::Close(tab_id("tab-d")))
            .is_err(),
        "a failed batch holds the reducer until the current resource is retried"
    );

    controller
        .complete_resource_operation(second.operation_id(), Ok(()))
        .unwrap();
    let third = controller.advance_close_batch().unwrap().unwrap();
    assert!(matches!(
        &third,
        WorkspaceResourceOperation::CloseTab {
            tab_id: actual_tab_id,
            ..
        } if actual_tab_id == &tab_id("tab-c")
    ));
    controller
        .complete_resource_operation(third.operation_id(), Ok(()))
        .unwrap();
    assert!(controller.advance_close_batch().unwrap().is_none());
    assert_eq!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().clone())
            .collect::<Vec<_>>(),
        [tab_id("tab-d")]
    );
    assert!(
        !controller
            .snapshot()
            .tab_groups
            .contains_key(&WorkspaceTabGroupId::parse("group-a").unwrap())
    );
}

/// 空组不创建孤儿状态，明确“关闭组”即可删除其目录元数据。
#[test]
fn closing_an_empty_group_deletes_its_metadata_without_resource_work() {
    let mut snapshot = grouped_tabs_snapshot();
    let empty_group = WorkspaceTabGroupId::parse("group-empty").unwrap();
    snapshot.tab_groups.insert(
        empty_group.clone(),
        WorkspaceTabGroup {
            title: "Empty".to_owned(),
            color: None,
            collapsed: false,
        },
    );
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::CloseBatch(
                WorkspaceCloseBatchIntent::Group {
                    group_id: "group-empty".to_owned(),
                    tab_ids: Vec::new(),
                },
            ))
            .unwrap(),
        WorkspaceControllerEffect::Changed { .. }
    ));
    assert!(!controller.snapshot().tab_groups.contains_key(&empty_group));
    assert!(controller.pending_resource_operation().is_none());
}

/// 确认弹层打开后若组成员发生变化，controller 拒绝旧点击快照而不关闭任何标签。
#[test]
fn stale_close_group_snapshot_is_rejected_after_membership_changes() {
    let snapshot = grouped_tabs_snapshot();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    controller
        .apply_tab_event(WorkspaceTabEvent::MoveToGroup {
            tab_id: tab_id("tab-d"),
            group_id: Some("group-a".to_owned()),
        })
        .unwrap();
    let stale_intent = WorkspaceCloseBatchIntent::Group {
        group_id: "group-a".to_owned(),
        tab_ids: vec![tab_id("tab-a"), tab_id("tab-b"), tab_id("tab-c")],
    };

    assert!(
        controller
            .apply_tab_event(WorkspaceTabEvent::CloseBatch(stale_intent))
            .is_err()
    );
    assert_eq!(controller.snapshot().terminal_tabs.len(), 4);
    assert!(controller.pending_resource_operation().is_none());
}

/// 关闭右侧和关闭其他都必须采用显示顺序中的精确锚点快照，不能由 controller 扩大目标。
#[test]
fn close_right_and_close_others_require_exact_anchor_snapshots() {
    let snapshot = grouped_tabs_snapshot();
    let new_controller = || {
        WorkspaceController::from_loaded(
            document_for(&snapshot),
            WorkspaceSessionRevision::new("revision-a"),
        )
        .unwrap()
    };
    let close_right = WorkspaceCloseBatchIntent::ToRight {
        anchor_tab_id: tab_id("tab-a"),
        tab_ids: vec![tab_id("tab-b"), tab_id("tab-c"), tab_id("tab-d")],
    };
    let mut controller = new_controller();
    let WorkspaceControllerEffect::ResourceOperation(first_right) = controller
        .apply_tab_event(WorkspaceTabEvent::CloseBatch(close_right.clone()))
        .unwrap()
    else {
        panic!("a non-empty right-side close must start the first exact target");
    };
    assert!(matches!(
        &first_right,
        WorkspaceResourceOperation::CloseTab {
            tab_id: actual_tab_id,
            ..
        } if actual_tab_id == &tab_id("tab-b")
    ));

    let mut stale_right = new_controller();
    assert!(
        stale_right
            .apply_tab_event(WorkspaceTabEvent::CloseBatch(
                WorkspaceCloseBatchIntent::ToRight {
                    anchor_tab_id: tab_id("tab-a"),
                    tab_ids: vec![tab_id("tab-b"), tab_id("tab-c")],
                }
            ))
            .is_err()
    );
    assert!(stale_right.pending_resource_operation().is_none());

    let close_others = WorkspaceCloseBatchIntent::Others {
        keep_tab_id: tab_id("tab-b"),
        tab_ids: vec![tab_id("tab-a"), tab_id("tab-c"), tab_id("tab-d")],
    };
    let mut controller = new_controller();
    let WorkspaceControllerEffect::ResourceOperation(first_others) = controller
        .apply_tab_event(WorkspaceTabEvent::CloseBatch(close_others))
        .unwrap()
    else {
        panic!("a non-empty close-others action must start the first exact target");
    };
    assert!(matches!(
        &first_others,
        WorkspaceResourceOperation::CloseTab {
            tab_id: actual_tab_id,
            ..
        } if actual_tab_id == &tab_id("tab-a")
    ));
}

/// 组内菜单移动按稳定身份交换相邻成员，边界或过期组目标不改会话与布局 owner。
///
/// @author kongweiguang
#[test]
fn move_within_group_uses_stable_id_and_preserves_terminal_resources() {
    let snapshot = grouped_tabs_snapshot();
    let original_panes = snapshot.terminal_panes.clone();
    let original_active_tab_id = snapshot.active_tab_id.clone();
    let original_focused_pane_id = snapshot.focused_pane_id.clone();
    let original_selected_machine_id = snapshot.selected_machine_id.clone();
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::MoveWithinGroup {
                tab_id: tab_id("tab-b"),
                group_id: "group-a".to_owned(),
                direction: GroupTabMoveDirection::Earlier,
            })
            .unwrap(),
        WorkspaceControllerEffect::Changed { .. }
    ));
    assert_eq!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-b", "tab-a", "tab-c", "tab-d"]
    );
    assert_eq!(controller.snapshot().terminal_panes, original_panes);
    assert_eq!(controller.snapshot().active_tab_id, original_active_tab_id);
    assert_eq!(
        controller.snapshot().focused_pane_id,
        original_focused_pane_id
    );
    assert_eq!(
        controller.snapshot().selected_machine_id,
        original_selected_machine_id
    );

    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::MoveWithinGroup {
                tab_id: tab_id("tab-b"),
                group_id: "group-a".to_owned(),
                direction: GroupTabMoveDirection::Later,
            })
            .unwrap(),
        WorkspaceControllerEffect::Changed { .. }
    ));
    assert_eq!(
        controller.snapshot().terminal_tabs[0].id(),
        &tab_id("tab-a")
    );

    controller
        .apply_tab_event(WorkspaceTabEvent::MoveToGroup {
            tab_id: tab_id("tab-b"),
            group_id: None,
        })
        .unwrap();
    assert!(
        controller
            .apply_tab_event(WorkspaceTabEvent::MoveWithinGroup {
                tab_id: tab_id("tab-b"),
                group_id: "group-a".to_owned(),
                direction: GroupTabMoveDirection::Earlier,
            })
            .is_err()
    );
}

/// 组范围批量关闭复用逐项资源 ACK，并拒绝成员变化或伪造的保留组快照。
///
/// @author kongweiguang
#[test]
fn group_scoped_close_batches_validate_membership_and_start_exact_targets() {
    let snapshot = grouped_tabs_snapshot();
    let close_outside = WorkspaceCloseBatchIntent::OutsideGroup {
        group_id: "group-a".to_owned(),
        keep_tab_ids: vec![tab_id("tab-a"), tab_id("tab-b"), tab_id("tab-c")],
        tab_ids: vec![tab_id("tab-d")],
    };
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let WorkspaceControllerEffect::ResourceOperation(outside_operation) = controller
        .apply_tab_event(WorkspaceTabEvent::CloseBatch(close_outside.clone()))
        .unwrap()
    else {
        panic!("close outside group must use the existing resource close pipeline");
    };
    assert!(matches!(
        &outside_operation,
        WorkspaceResourceOperation::CloseTab { tab_id: actual, .. }
            if actual == &tab_id("tab-d")
    ));
    controller
        .complete_resource_operation(
            outside_operation.operation_id(),
            Err(WorkspaceRestoreFailure::ResourceFailure),
        )
        .unwrap();
    assert_eq!(
        controller.retry_pending_close_batch(&close_outside),
        Some(outside_operation.clone()),
        "retry must preserve the same group identity and exact remaining target"
    );

    let stale_outside = WorkspaceCloseBatchIntent::OutsideGroup {
        group_id: "group-a".to_owned(),
        keep_tab_ids: vec![tab_id("tab-a"), tab_id("tab-b")],
        tab_ids: vec![tab_id("tab-c"), tab_id("tab-d")],
    };
    let mut stale_controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    assert!(
        stale_controller
            .apply_tab_event(WorkspaceTabEvent::CloseBatch(stale_outside))
            .is_err()
    );
    assert!(stale_controller.pending_resource_operation().is_none());

    let close_group_others = WorkspaceCloseBatchIntent::GroupOthers {
        group_id: "group-a".to_owned(),
        keep_tab_id: tab_id("tab-b"),
        tab_ids: vec![tab_id("tab-a"), tab_id("tab-c")],
    };
    let mut controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    let WorkspaceControllerEffect::ResourceOperation(group_operation) = controller
        .apply_tab_event(WorkspaceTabEvent::CloseBatch(close_group_others.clone()))
        .unwrap()
    else {
        panic!("close same-group others must use the existing resource close pipeline");
    };
    assert!(matches!(
        &group_operation,
        WorkspaceResourceOperation::CloseTab { tab_id: actual, .. }
            if actual == &tab_id("tab-a")
    ));
    controller
        .complete_resource_operation(
            group_operation.operation_id(),
            Err(WorkspaceRestoreFailure::ResourceFailure),
        )
        .unwrap();
    assert_eq!(
        controller.retry_pending_close_batch(&close_group_others),
        Some(group_operation.clone()),
        "group-others retry must retain its group and keep-tab anchors"
    );

    let invalid_group_anchor = WorkspaceCloseBatchIntent::GroupOthers {
        group_id: "group-a".to_owned(),
        keep_tab_id: tab_id("tab-d"),
        tab_ids: vec![tab_id("tab-a"), tab_id("tab-c")],
    };
    let mut stale_controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();
    assert!(
        stale_controller
            .apply_tab_event(WorkspaceTabEvent::CloseBatch(invalid_group_anchor))
            .is_err()
    );
    assert!(stale_controller.pending_resource_operation().is_none());
}

/// 拖放候选仍复用 CAS 保存门禁；未知提交必须显式回读，确认未提交后才允许重试。
#[test]
fn unknown_save_requires_readback_before_explicit_retry() {
    let mut controller = WorkspaceController::from_missing().unwrap();
    let mut snapshot = one_terminal_snapshot();
    snapshot
        .terminal_tabs
        .push(transfer_tab_with("tab-b", "B", None));
    controller.synchronize_snapshot(snapshot.clone()).unwrap();
    controller
        .apply_tab_event(WorkspaceTabEvent::DropOnTab {
            tab_id: tab_id("tab-a"),
            target_tab_id: tab_id("tab-b"),
            placement: TabDropPlacement::After,
        })
        .unwrap();
    let request = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        request
            .candidate_document
            .workspace_snapshot()
            .unwrap()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-b", "tab-a"]
    );
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::OutcomeUnknown,
        )
        .unwrap();

    assert!(controller.begin_save().unwrap().is_none());
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::OutcomeUnknown { .. }
    ));
    controller
        .resolve_unknown_readback(WorkspaceSessionLoad::Missing)
        .unwrap();
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
    ));
    controller.retry_after_readback().unwrap();
    let retry = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        retry.expected_revision,
        WorkspaceSessionExpectedRevision::Missing
    );
    assert_eq!(
        retry
            .candidate_document
            .workspace_snapshot()
            .unwrap()
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().as_str())
            .collect::<Vec<_>>(),
        ["tab-b", "tab-a"]
    );
}

/// 缺失目标在载入时明确失败； Root 不能将其当作默认本地目标重连。
#[test]
fn restore_status_preserves_unavailable_target_failure() {
    let mut snapshot = one_terminal_snapshot();
    snapshot.terminal_panes[0].target = None;
    let controller = WorkspaceController::from_loaded(
        document_for(&snapshot),
        WorkspaceSessionRevision::new("revision-a"),
    )
    .unwrap();

    assert_eq!(
        controller.restore_status(&WorkspaceRestoreItemId::Pane(pane_id("pane-a"))),
        Some(WorkspaceRestoreStatus::Failed(
            WorkspaceRestoreFailure::MissingTarget
        ))
    );
}
