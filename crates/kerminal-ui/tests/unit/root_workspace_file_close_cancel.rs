//! 文件关闭取消必须保留原工作区，并且只释放匹配的资源候选。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_application::workspace::{
    WorkspaceFileAccess, WorkspaceFileSource, WorkspaceSessionDocument, WorkspaceSessionRevision,
    WorkspaceSessionSnapshot, WorkspaceTargetRef,
};

use super::*;

/// 固定标签身份让测试能够区分取消目标、其他文件和非文件资源。
fn tab_id(raw: &str) -> TabId {
    TabId::parse(raw).expect("valid tab ID")
}

/// 文件描述符使用真实持久化类型，避免测试绕过 controller 的快照校验。
fn file_tab(id: &str, path: &str) -> WorkspaceSessionTab {
    WorkspaceSessionTab::WorkspaceFile {
        id: tab_id(id),
        tab_group_id: None,
        title: id.to_owned(),
        machine_id: "machine-local".to_owned(),
        target: WorkspaceTargetRef::Local { profile_id: None },
        path: path.to_owned(),
        root_path: Some("C:/work".to_owned()),
        access: WorkspaceFileAccess::Editable,
        source: WorkspaceFileSource::Local,
    }
}

/// 保留非文件标签以证明迟到回执不会释放其他类型的资源操作。
fn transfer_tab() -> WorkspaceSessionTab {
    WorkspaceSessionTab::SftpTransfer {
        id: tab_id("transfer"),
        tab_group_id: None,
        title: "Transfer".to_owned(),
        machine_id: "machine-local".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: None,
        initial_right_selection: None,
    }
}

/// 从已持久化的有效快照构造 controller，取消前后的比较因此包含标签顺序和活动身份。
fn controller() -> WorkspaceController {
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id("file-a")),
        focused_pane_id: None,
        selected_machine_id: Some("machine-local".to_owned()),
        terminal_tabs: vec![
            file_tab("file-a", "C:/work/a.txt"),
            file_tab("file-b", "C:/work/b.txt"),
            transfer_tab(),
        ],
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    };
    snapshot.validate().expect("valid workspace fixture");
    let mut document = WorkspaceSessionDocument::empty_v3();
    document
        .update_known_fields(&snapshot)
        .expect("encode workspace fixture");
    WorkspaceController::from_loaded(document, WorkspaceSessionRevision::new("fixture-revision"))
        .expect("load workspace fixture")
}

/// 关闭事件必须进入资源门禁，后续断言才能验证原操作是否仍被正确持有。
fn begin_close(controller: &mut WorkspaceController, id: &TabId) -> WorkspaceResourceOperation {
    let WorkspaceControllerEffect::ResourceOperation(operation) = controller
        .apply_tab_event(WorkspaceTabEvent::Close(id.clone()))
        .expect("begin close")
    else {
        panic!("tab close must wait for its resource acknowledgement");
    };
    operation
}

/// 取消文件关闭后不提交候选，导航和其他文件关闭可以立即继续。
#[test]
fn cancelled_file_close_keeps_original_snapshot_and_releases_navigation() {
    let mut controller = controller();
    let original = controller.snapshot().clone();
    let file_a = tab_id("file-a");
    let file_b = tab_id("file-b");
    let operation = begin_close(&mut controller, &file_a);

    assert!(cancel_unclosed_file_tab_operation(
        &mut controller,
        operation.operation_id(),
        &file_a,
    ));
    assert_eq!(controller.snapshot(), &original);
    assert!(controller.pending_resource_operation().is_none());
    assert!(matches!(
        controller
            .apply_tab_event(WorkspaceTabEvent::Activate(file_b.clone()))
            .expect("another tab can be activated after cancellation"),
        WorkspaceControllerEffect::Changed { .. }
    ));
    let other_close = begin_close(&mut controller, &file_b);
    controller
        .complete_resource_operation(other_close.operation_id(), Ok(()))
        .expect("another file can close after cancellation");
    assert!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .any(|tab| tab.id() == &file_a),
        "the cancelled file keeps its original tab identity"
    );
}

/// 过期操作、错误标签和非文件关闭都不能清除当前资源 owner。
#[test]
fn stale_or_non_file_cancellation_preserves_the_pending_operation() {
    let mut controller = controller();
    let file_a = tab_id("file-a");
    let operation = begin_close(&mut controller, &file_a);

    assert!(!cancel_unclosed_file_tab_operation(
        &mut controller,
        "stale-operation",
        &file_a,
    ));
    assert!(!cancel_unclosed_file_tab_operation(
        &mut controller,
        operation.operation_id(),
        &tab_id("file-b"),
    ));
    assert_eq!(controller.pending_resource_operation(), Some(&operation));
    assert!(cancel_unclosed_file_tab_operation(
        &mut controller,
        operation.operation_id(),
        &file_a,
    ));

    let transfer = tab_id("transfer");
    let transfer_operation = begin_close(&mut controller, &transfer);
    assert!(!cancel_unclosed_file_tab_operation(
        &mut controller,
        transfer_operation.operation_id(),
        &transfer,
    ));
    assert_eq!(
        controller.pending_resource_operation(),
        Some(&transfer_operation)
    );
    controller
        .complete_resource_operation(transfer_operation.operation_id(), Ok(()))
        .expect("the retained non-file operation can still complete");
}

/// 批次中途取消保留已完成结果，停止剩余目标并解除后续导航门禁。
#[test]
fn cancelled_file_close_stops_the_remaining_batch() {
    let mut controller = controller();
    let file_a = tab_id("file-a");
    let file_b = tab_id("file-b");
    let transfer = tab_id("transfer");
    let intent = WorkspaceCloseBatchIntent::Others {
        keep_tab_id: transfer.clone(),
        tab_ids: vec![file_a.clone(), file_b.clone()],
    };
    let WorkspaceControllerEffect::ResourceOperation(first) = controller
        .apply_tab_event(WorkspaceTabEvent::CloseBatch(intent))
        .expect("begin batch")
    else {
        panic!("batch close must start the first resource operation");
    };
    controller
        .complete_resource_operation(first.operation_id(), Ok(()))
        .expect("first close completed");
    let second = controller
        .advance_close_batch()
        .expect("advance batch")
        .expect("second file remains");
    let after_first = controller.snapshot().clone();

    assert!(cancel_unclosed_file_tab_operation(
        &mut controller,
        second.operation_id(),
        &file_b,
    ));
    assert_eq!(controller.snapshot(), &after_first);
    assert!(controller.pending_resource_operation().is_none());
    assert!(controller.pending_close_batch_remaining_count().is_none());
    assert!(controller.advance_close_batch().unwrap().is_none());
    assert!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .all(|tab| tab.id() != &file_a)
    );
    assert!(
        controller
            .snapshot()
            .terminal_tabs
            .iter()
            .any(|tab| tab.id() == &file_b)
    );
    controller
        .apply_tab_event(WorkspaceTabEvent::Activate(transfer))
        .expect("navigation remains available after batch cancellation");
}
