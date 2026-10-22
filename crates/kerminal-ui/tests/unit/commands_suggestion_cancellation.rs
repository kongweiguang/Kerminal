//! CommandsView 建议查询的协作取消边界。
//!
//! @author kongweiguang

use tokio_util::sync::CancellationToken;

use super::cancel_owned_suggestion_request;

/// 释放 view 所有的请求时必须通知 provider 并清空槽位，避免旧代次仍占用资源。
#[test]
fn cancelling_owned_request_notifies_provider_and_releases_slot() {
    let request = CancellationToken::new();
    let provider = request.clone();
    let mut owned = Some(request);

    cancel_owned_suggestion_request(&mut owned);

    assert!(provider.is_cancelled());
    assert!(owned.is_none());
}
