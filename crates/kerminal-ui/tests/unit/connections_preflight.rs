//! SSH 连接预检的请求取消与迟到结果隔离测试。
//!
//! @author kongweiguang

use tokio_util::sync::CancellationToken;

use super::ConnectionRequestEpoch;

/// 新请求会取消旧远端令牌，并且旧 generation 不能再发布挑战或连接动作。
#[test]
fn starting_new_request_cancels_old_preflight_and_rejects_its_result() {
    let mut epoch = ConnectionRequestEpoch::new();
    let executor_token = CancellationToken::new();
    let (old_generation, old_request) = epoch.begin();
    let old_operation = old_request.bind_to(&executor_token);
    assert!(epoch.is_current(old_generation, &old_request));
    assert!(!old_operation.is_cancelled());

    let (new_generation, new_request) = epoch.begin();

    assert!(old_operation.is_cancelled());
    assert!(!epoch.is_current(old_generation, &old_request));
    assert!(epoch.is_current(new_generation, &new_request));
}

/// 取消发生在任务启动前也会传递；应用 executor 关闭同样会取消已绑定的操作令牌。
#[test]
fn cancellation_before_bind_and_executor_shutdown_reach_operation() {
    let mut epoch = ConnectionRequestEpoch::new();
    let (cancelled_generation, cancelled_request) = epoch.begin();
    epoch.cancel();

    let executor_token = CancellationToken::new();
    let cancelled_operation = cancelled_request.bind_to(&executor_token);
    assert!(cancelled_operation.is_cancelled());
    assert!(!epoch.is_current(cancelled_generation, &cancelled_request));

    let (active_generation, active_request) = epoch.begin();
    let executor_token = CancellationToken::new();
    let active_operation = active_request.bind_to(&executor_token);
    executor_token.cancel();

    assert!(active_operation.is_cancelled());
    assert!(!epoch.is_current(active_generation, &active_request));
}
