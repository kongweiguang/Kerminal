//! 偏好页同步取消句柄的协作取消测试。
//!
//! @author kongweiguang

use super::SyncCancellationHandle;
use tokio_util::sync::CancellationToken;

/// UI 先请求取消的竞态会在 executor 分配 token 后立即传给 application 用例。
#[test]
fn cancellation_requested_before_token_install_is_forwarded() {
    let handle = SyncCancellationHandle::default();
    assert!(handle.request_cancel());
    assert!(!handle.request_cancel());

    let token = CancellationToken::new();
    handle.install(token.clone());

    assert!(token.is_cancelled());
    assert!(handle.cancel_requested());
}

/// token 已安装时取消只发出协作请求，不能把 UI 状态伪装成任务已经结束。
#[test]
fn cancellation_after_token_install_reaches_application_token() {
    let handle = SyncCancellationHandle::default();
    let token = CancellationToken::new();
    handle.install(token.clone());

    assert!(handle.request_cancel());
    assert!(token.is_cancelled());
    assert!(handle.cancel_requested());
}
