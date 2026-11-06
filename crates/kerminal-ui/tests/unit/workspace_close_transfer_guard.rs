//! 工作区关闭门禁覆盖所有传输来源与迟到回执身份。
//!
//! @author kongweiguang

use crate::features::{
    workspace_controller::WorkspaceResourceOperation,
    workspace_tabs::{WorkspaceCloseBatchIntent, WorkspaceTabEvent},
};
use crate::root::workspace_close::{
    PendingTransferCloseAction, PendingTransferCloseGuard, TransferCloseGuardDecision,
    should_start_transfer_close_guard, transfer_close_guard_decision,
    transfer_close_targets_for_event, transfer_close_targets_for_operation,
};
use kerminal_application::workspace::{
    WorkspaceFileAccess, WorkspaceFileSource, WorkspaceSessionTab, WorkspaceTargetRef,
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::TerminalLayoutNode,
};

/// 构造真实 SFTP descriptor，验证门禁按稳定 TabId 绑定来源。
///
/// @author kongweiguang
fn transfer_tab(id: TabId, title: &str) -> WorkspaceSessionTab {
    WorkspaceSessionTab::SftpTransfer {
        id,
        tab_group_id: None,
        title: title.to_owned(),
        machine_id: "local".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: None,
        initial_right_selection: None,
    }
}

/// 文件 tab 用独立 WorkspaceFile origin，不能与同主机 SFTP 历史合并。
///
/// @author kongweiguang
fn workspace_file_tab(id: TabId, title: &str) -> WorkspaceSessionTab {
    WorkspaceSessionTab::WorkspaceFile {
        id,
        tab_group_id: None,
        title: title.to_owned(),
        machine_id: "local".to_owned(),
        target: WorkspaceTargetRef::Local { profile_id: None },
        path: "notes.txt".to_owned(),
        root_path: None,
        access: WorkspaceFileAccess::Editable,
        source: WorkspaceFileSource::Workspace,
    }
}

/// 批次预检包含每个终端 pane、SFTP transfer 和 workspace file 来源，并跳过共享工作台。
///
/// @author kongweiguang
#[test]
fn batch_close_freezes_all_persistent_transfer_origins() {
    let transfer_id = TabId::new();
    let terminal_id = TabId::new();
    let file_id = TabId::new();
    let workbench_id = TabId::new();
    let pane_id = PaneId::new();
    let terminal = WorkspaceSessionTab::Terminal {
        id: terminal_id.clone(),
        tab_group_id: None,
        title: "Terminal".to_owned(),
        machine_id: "local".to_owned(),
        layout: TerminalLayoutNode::Pane {
            pane_id: pane_id.clone(),
        },
    };
    let transfer = transfer_tab(transfer_id.clone(), "SFTP");
    let file = workspace_file_tab(file_id.clone(), "File");
    let tabs = vec![terminal.clone(), transfer.clone(), file.clone()];
    let event = WorkspaceTabEvent::CloseBatch(WorkspaceCloseBatchIntent::ToRight {
        anchor_tab_id: TabId::new(),
        tab_ids: vec![
            workbench_id.clone(),
            terminal_id,
            transfer_id.clone(),
            file_id.clone(),
        ],
    });

    let targets = transfer_close_targets_for_event(&event, &tabs, &workbench_id);
    assert_eq!(targets.len(), 3);
    assert!(targets.iter().any(|target| {
        target.origin
            == kerminal_application::transfers::TransferOrigin::TerminalPane(pane_id.clone())
    }));
    assert!(targets.iter().any(|target| {
        target.origin
            == kerminal_application::transfers::TransferOrigin::SftpWorkspace(transfer_id.clone())
    }));
    assert!(targets.iter().any(|target| {
        target.origin
            == kerminal_application::transfers::TransferOrigin::WorkspaceFile(file_id.clone())
    }));
    assert!(
        transfer_close_targets_for_event(
            &WorkspaceTabEvent::Activate(terminal.id().clone()),
            &tabs,
            &workbench_id,
        )
        .is_empty()
    );

    let close_pane = WorkspaceResourceOperation::ClosePane {
        operation_id: "close-pane".to_owned(),
        tab_id: terminal.id().clone(),
        pane_id: pane_id.clone(),
    };
    let pane_targets = transfer_close_targets_for_operation(&close_pane, &tabs);
    assert_eq!(pane_targets.len(), 1);
    assert_eq!(
        pane_targets[0].origin,
        kerminal_application::transfers::TransferOrigin::TerminalPane(pane_id)
    );

    let close_file = WorkspaceResourceOperation::CloseTab {
        operation_id: "close-file".to_owned(),
        tab_id: file_id.clone(),
        tab: Box::new(file),
        pane_ids: Vec::new(),
    };
    let file_targets = transfer_close_targets_for_operation(&close_file, &tabs);
    assert_eq!(file_targets.len(), 1);
    assert_eq!(
        file_targets[0].origin,
        kerminal_application::transfers::TransferOrigin::WorkspaceFile(file_id)
    );
}

/// 异步回执只有同一代次与仍存在的完整 descriptor 才能继续关闭。
///
/// @author kongweiguang
#[test]
fn close_guard_rejects_stale_generation_or_replaced_targets() {
    let target = transfer_tab(TabId::new(), "SFTP");
    let event = WorkspaceTabEvent::Close(target.id().clone());
    let workbench_id = TabId::new();
    let targets =
        transfer_close_targets_for_event(&event, std::slice::from_ref(&target), &workbench_id);
    let ticket = PendingTransferCloseGuard::new(
        12,
        PendingTransferCloseAction::WorkspaceEvent(event),
        targets,
    );
    assert!(ticket.is_current(12, std::slice::from_ref(&target)));
    assert!(!ticket.is_current(11, std::slice::from_ref(&target)));

    let replaced = transfer_tab(target.id().clone(), "Replaced descriptor");
    assert!(!ticket.is_current(12, &[replaced]));
    assert!(!ticket.is_current(12, &[]));

    assert!(should_start_transfer_close_guard(false, true, 1));
    assert!(!should_start_transfer_close_guard(false, false, 1));
    assert!(!should_start_transfer_close_guard(true, true, 1));
    assert!(!should_start_transfer_close_guard(false, true, 0));
    assert!(matches!(
        transfer_close_guard_decision(Ok(Vec::new())),
        TransferCloseGuardDecision::Allow
    ));
    assert!(matches!(
        transfer_close_guard_decision(Ok(vec![ticket.targets[0].clone()])),
        TransferCloseGuardDecision::Blocked(_)
    ));
    assert!(matches!(
        transfer_close_guard_decision(Err(kerminal_application::AppError::cancelled(
            "inspection failed",
        ))),
        TransferCloseGuardDecision::QueryFailed(_)
    ));
}
