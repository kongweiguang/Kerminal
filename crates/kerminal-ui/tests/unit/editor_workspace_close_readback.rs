//! 编辑器关闭读回取消、失败和普通重试的状态回归。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::files::FileEndpoint;
use tokio_util::sync::CancellationToken;

use super::{
    EditorConfirmation, EditorQuitGate, FileEditorWorkspaceEvent, PendingEditorSave,
    PendingWorkspaceClose, ReadbackCloseIntent, can_replace, confirmation_endpoint,
    evaluate_editor_quit_gate, take_workspace_close_cancellation, take_workspace_close_resolution,
};

/// 构造稳定的本地文件身份，让收据测试聚焦 endpoint 与读取代次匹配。
fn test_endpoint() -> FileEndpoint {
    FileEndpoint::Local {
        path: PathBuf::from("C:/workspace/readback.txt"),
    }
}

/// 取消当前确认读回会发出失败收据，普通重试不能继承关闭动作。
#[test]
fn cancelled_readback_resolves_close_false_and_retry_keeps_editor_tab() {
    let endpoint = test_endpoint();
    let mut pending_close = Some(PendingWorkspaceClose {
        operation_id: "close-1".to_owned(),
        endpoint: endpoint.clone(),
        load_generation: Some(11),
    });
    let mut close_intent = ReadbackCloseIntent::AwaitingConfirmation;
    assert!(close_intent.bind_to_load(11));

    let close_tab = close_intent.cancel_load(11);
    let receipt = take_workspace_close_resolution(&mut pending_close, &endpoint, Some(11), false);

    assert!(!close_tab);
    assert_eq!(close_intent, ReadbackCloseIntent::None);
    assert_eq!(
        receipt,
        Some(FileEditorWorkspaceEvent::WorkspaceTabCloseResolved {
            operation_id: "close-1".to_owned(),
            endpoint: endpoint.clone(),
            closed: false,
        })
    );
    assert!(pending_close.is_none());

    assert!(!close_intent.bind_to_load(12));
    assert!(!close_intent.finish_load(12, true));
    assert!(pending_close.is_none());
}

/// 读回失败同样清理原关闭意图并保留文档，下一次普通成功读取不会删除编辑器标签。
#[test]
fn failed_readback_resolves_close_false_and_retry_is_not_close_authorized() {
    let endpoint = test_endpoint();
    let mut pending_close = Some(PendingWorkspaceClose {
        operation_id: "close-2".to_owned(),
        endpoint: endpoint.clone(),
        load_generation: Some(21),
    });
    let mut close_intent = ReadbackCloseIntent::AwaitingConfirmation;
    assert!(close_intent.bind_to_load(21));

    let close_tab = close_intent.finish_load(21, false);
    let receipt = take_workspace_close_resolution(&mut pending_close, &endpoint, Some(21), false);

    assert!(!close_tab);
    assert_eq!(close_intent, ReadbackCloseIntent::None);
    assert!(matches!(
        receipt,
        Some(FileEditorWorkspaceEvent::WorkspaceTabCloseResolved { closed: false, .. })
    ));
    assert!(pending_close.is_none());

    assert!(!close_intent.bind_to_load(22));
    assert!(!close_intent.finish_load(22, true));
}

/// 迟到 generation 或其它文件的关闭回执不能消费当前 workspace close owner。
#[test]
fn close_receipt_requires_same_endpoint_and_load_generation() {
    let endpoint = test_endpoint();
    let mut pending_close = Some(PendingWorkspaceClose {
        operation_id: "close-3".to_owned(),
        endpoint: endpoint.clone(),
        load_generation: Some(31),
    });
    let different_endpoint = FileEndpoint::Local {
        path: PathBuf::from("C:/workspace/other.txt"),
    };

    assert!(
        take_workspace_close_resolution(&mut pending_close, &endpoint, Some(30), false,).is_none()
    );
    assert!(
        take_workspace_close_resolution(&mut pending_close, &different_endpoint, Some(31), false,)
            .is_none()
    );
    assert!(pending_close.is_some());
    assert!(
        take_workspace_close_resolution(&mut pending_close, &endpoint, Some(31), false,).is_some()
    );
}

/// Quit(A) 抢占 Close(B) 后仍按 B 的原 owner 回报未关闭，取消 A 不会遗留 B 的挂起操作。
/// 回归只操作纯确认与回执状态，既不移除文档也不把仍有脏内容的退出门禁误判为 Ready。
///
/// @author kongweiguang
#[test]
fn quit_releases_original_workspace_close_when_confirmation_moves_to_another_file() {
    let endpoint_b = test_endpoint();
    let endpoint_a = FileEndpoint::Local {
        path: PathBuf::from("C:/workspace/first-dirty.txt"),
    };
    let confirmation = EditorConfirmation::Quit(endpoint_a);
    let mut pending_close = Some(PendingWorkspaceClose {
        operation_id: "close-b-before-quit".to_owned(),
        endpoint: endpoint_b.clone(),
        load_generation: Some(51),
    });

    assert!(
        take_workspace_close_resolution(
            &mut pending_close,
            confirmation_endpoint(&confirmation),
            Some(51),
            false,
        )
        .is_none(),
        "the current Quit(A) confirmation cannot settle the original Close(B) owner"
    );
    assert!(pending_close.is_some());
    assert_eq!(
        take_workspace_close_cancellation(&mut pending_close),
        Some(FileEditorWorkspaceEvent::WorkspaceTabCloseResolved {
            operation_id: "close-b-before-quit".to_owned(),
            endpoint: endpoint_b.clone(),
            closed: false,
        })
    );
    assert!(pending_close.is_none());
    assert!(take_workspace_close_cancellation(&mut pending_close).is_none());
    assert_eq!(
        evaluate_editor_quit_gate(false, false, false, true),
        EditorQuitGate::ConfirmUnsaved,
        "cancelling a workspace close must not authorize discarding dirty contents"
    );

    let mut next_close = Some(PendingWorkspaceClose {
        operation_id: "close-b-after-quit-cancel".to_owned(),
        endpoint: endpoint_b.clone(),
        load_generation: Some(52),
    });
    assert!(
        take_workspace_close_resolution(
            &mut next_close,
            confirmation_endpoint(&confirmation),
            Some(51),
            false,
        )
        .is_none(),
        "late cancellation for A must not consume the next B close owner"
    );
    assert_eq!(
        take_workspace_close_resolution(&mut next_close, &endpoint_b, Some(52), false),
        Some(FileEditorWorkspaceEvent::WorkspaceTabCloseResolved {
            operation_id: "close-b-after-quit-cancel".to_owned(),
            endpoint: endpoint_b,
            closed: false,
        })
    );
    assert!(next_close.is_none());
}

/// 替换仅在加载完成、权限可写且 GPUI 编辑器已初始化时可用。
#[test]
fn replacement_is_disabled_while_loading_readonly_or_without_editor() {
    assert!(!can_replace(true, false, true));
    assert!(!can_replace(false, true, true));
    assert!(!can_replace(false, false, false));
    assert!(can_replace(false, false, true));
}

/// 保存取消只标记同文件同代次请求，保存 owner 仍保留到实际终态回执。
#[test]
fn save_cancel_request_keeps_endpoint_generation_owner_until_receipt() {
    let endpoint = test_endpoint();
    let cancellation = CancellationToken::new();
    let mut operation = PendingEditorSave::new(endpoint.clone(), 41, cancellation.clone());
    let other_endpoint = FileEndpoint::Local {
        path: PathBuf::from("C:/workspace/other.txt"),
    };

    assert!(operation.matches(&endpoint, 41));
    assert!(operation.request_cancel());
    assert!(cancellation.is_cancelled());
    assert!(operation.cancel_requested());
    assert!(operation.matches(&endpoint, 41));
    assert!(!operation.request_cancel(), "repeated cancel is idempotent");
    assert_eq!(
        evaluate_editor_quit_gate(false, true, false, true),
        EditorQuitGate::WaitingForSave,
        "a cancellation request must not satisfy the save/quit gate"
    );
    assert!(!operation.matches(&other_endpoint, 41));
    assert!(!operation.matches(&endpoint, 42));
}
