//! 扩展标签复用工作区保存和资源关闭门禁。
//!
//! @author kongweiguang

use kerminal_application::workspace::{
    WorkspaceSessionLoad, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
    WorkspaceSessionTab,
};
use kerminal_domain::identity::TabId;
use kerminal_ui::features::{
    workspace_controller::{
        WorkspaceController, WorkspaceControllerEffect, WorkspacePersistenceState,
        WorkspaceRestoreFailure, WorkspaceRestoreItem, WorkspaceRestoreItemId,
        WorkspaceRestoreStatus,
    },
    workspace_tabs::{WorkspaceTabEvent, WorkspaceTabKind},
};
use serde_json::{Map, json};

/// 保存结果未知时必须回读相同的扩展状态；关闭失败保留同一个资源操作供重试。
/// @author kongweiguang
#[test]
fn extension_tab_saves_restores_and_closes_with_existing_gates() {
    let tab_id = TabId::parse("plugin-tab").unwrap();
    let mut controller = WorkspaceController::from_missing().unwrap();
    let mut snapshot = controller.snapshot().clone();
    snapshot.active_tab_id = Some(tab_id.clone());
    snapshot.terminal_tabs.push(WorkspaceSessionTab::Extension {
        id: tab_id.clone(),
        tab_group_id: None,
        title: "Notes".to_owned(),
        content_id: "example.notes".to_owned(),
        state_version: 1,
        state: json!({"text": "saved"}),
        extra_fields: Map::new(),
    });
    controller.synchronize_snapshot(snapshot).unwrap();
    assert!(matches!(
        &controller.tab_projections()[0].kind,
        WorkspaceTabKind::Extension(id) if id.as_ref() == "example.notes"
    ));
    assert!(controller.tab_projections()[0].layout.is_none());
    assert!(
        !controller
            .update_plugin_tab_state(&tab_id, 1, json!({"text": "saved"}))
            .unwrap()
    );

    let request = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        request.candidate_document.as_value()["terminalTabs"][0]["state"]["text"],
        "saved"
    );
    assert!(
        controller
            .update_plugin_tab_state(&tab_id, 1, json!({"text": "new draft"}))
            .unwrap()
    );
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::OutcomeUnknown,
        )
        .unwrap();
    assert!(controller.begin_save().unwrap().is_none());
    controller
        .resolve_unknown_readback(WorkspaceSessionLoad::Loaded {
            document: request.candidate_document.clone(),
            revision: WorkspaceSessionRevision::new("readback-revision"),
        })
        .unwrap();
    assert_eq!(
        controller.persistence_state(),
        &WorkspacePersistenceState::Dirty
    );
    let second_request = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        second_request.candidate_document.as_value()["terminalTabs"][0]["state"]["text"],
        "new draft"
    );

    let mut restored = WorkspaceController::from_loaded(
        request.candidate_document,
        WorkspaceSessionRevision::new("restored-revision"),
    )
    .unwrap();
    assert!(
        matches!(&restored.restore_plan()[0], WorkspaceRestoreItem::Extension { tab }
        if matches!(tab.as_ref(), WorkspaceSessionTab::Extension { content_id, state, .. }
            if content_id == "example.notes" && state["text"] == "saved"))
    );
    assert_eq!(
        restored.restore_status(&WorkspaceRestoreItemId::Tab(tab_id.clone())),
        Some(WorkspaceRestoreStatus::Pending)
    );
    restored
        .record_restore_result(WorkspaceRestoreItemId::Tab(tab_id.clone()), Ok(()))
        .unwrap();
    let WorkspaceControllerEffect::ResourceOperation(operation) = restored
        .apply_tab_event(WorkspaceTabEvent::Close(tab_id.clone()))
        .unwrap()
    else {
        panic!("close must wait for the content instance");
    };
    restored
        .complete_resource_operation(
            operation.operation_id(),
            Err(WorkspaceRestoreFailure::ResourceFailure),
        )
        .unwrap();
    assert_eq!(restored.snapshot().terminal_tabs.len(), 1);
    assert_eq!(
        restored.retry_pending_resource_operation(),
        Some(operation.clone())
    );
    restored
        .complete_resource_operation(operation.operation_id(), Ok(()))
        .unwrap();
    assert!(restored.snapshot().terminal_tabs.is_empty());
}
