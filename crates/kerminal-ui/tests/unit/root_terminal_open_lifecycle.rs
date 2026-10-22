//! 生命周期 watcher 的异步回执按请求票据和服务代次排序。
//!
//! @author kongweiguang

use std::collections::HashMap;

use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{SessionLifecycle, SessionOwner},
};

use crate::root::terminal_open::{
    lifecycle_context_generation_is_current, lifecycle_context_request_is_current,
    lifecycle_retains_terminal_history, next_lifecycle_context_request_generation,
    register_lifecycle_context_request,
};

/// 用实际 UI session owner 测试异步回执是否仍对应同一个 pane。
///
/// @author kongweiguang
fn test_owner() -> SessionOwner {
    SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    }
}

/// 重启完成后，旧 Running 与 Starting 查询都不能抢占新 Running 请求或撤销新绑定。
///
/// @author kongweiguang
#[test]
fn delayed_lifecycle_results_are_fenced_by_restart_and_running_ticket() {
    let session_id = SessionId::new();
    let owner = test_owner();
    let mut request_generation = 0;
    let mut requests = HashMap::<SessionId, (u64, SessionOwner)>::new();

    let old_running_ticket = next_lifecycle_context_request_generation(&mut request_generation);
    requests.insert(session_id.clone(), (old_running_ticket, owner.clone()));
    let old_context_generation = 7;

    // restart 开始时使先前 Running 查询失效；Starting 查询可能仍在异步执行。
    requests.remove(&session_id);
    let starting_ticket = next_lifecycle_context_request_generation(&mut request_generation);
    requests.insert(session_id.clone(), (starting_ticket, owner.clone()));

    // restart 完成时再失效在途 Starting 查询，然后处理新的 Running 观察。
    requests.remove(&session_id);
    let current_service_generation = 8;
    let running_ticket = next_lifecycle_context_request_generation(&mut request_generation);
    requests.insert(session_id.clone(), (running_ticket, owner.clone()));

    // Ok(Some)、Ok(None)、Err 都在结果分支之前经过同一个票据门，旧回执无权修改 handler/status。
    for _result_kind in ["context", "empty", "error"] {
        assert!(!lifecycle_context_request_is_current(
            requests.get(&session_id),
            old_running_ticket,
            &owner,
        ));
    }
    assert!(!lifecycle_context_request_is_current(
        requests.get(&session_id),
        starting_ticket,
        &owner,
    ));
    assert!(lifecycle_context_request_is_current(
        requests.get(&session_id),
        running_ticket,
        &owner,
    ));

    // 旧 generation 不可覆盖 restart 后的输入 binding；初次创建尚无观察时则仍可安装。
    assert!(!lifecycle_context_generation_is_current(
        old_context_generation,
        Some(current_service_generation),
    ));
    assert!(lifecycle_context_generation_is_current(
        current_service_generation,
        Some(current_service_generation),
    ));
    assert!(lifecycle_context_generation_is_current(1, None));
}

/// This unit test covers only the shared owner-scoped ticket primitive, not the GPUI Ready callback or its service lookup.
///
/// @author kongweiguang
#[test]
fn newer_lifecycle_request_fences_the_initial_ready_ticket() {
    let session_id = SessionId::new();
    let owner = test_owner();
    let mut request_generation = 0;
    let mut requests = HashMap::<SessionId, (u64, SessionOwner)>::new();

    let ready_ticket = register_lifecycle_context_request(
        &mut request_generation,
        &mut requests,
        session_id.clone(),
        owner.clone(),
    );
    assert!(lifecycle_context_request_is_current(
        requests.get(&session_id),
        ready_ticket,
        &owner,
    ));

    let lifecycle_ticket = register_lifecycle_context_request(
        &mut request_generation,
        &mut requests,
        session_id.clone(),
        owner.clone(),
    );
    assert!(!lifecycle_context_request_is_current(
        requests.get(&session_id),
        ready_ticket,
        &owner,
    ));
    assert!(lifecycle_context_request_is_current(
        requests.get(&session_id),
        lifecycle_ticket,
        &owner,
    ));
}

/// Exited/Failed 保留只读历史，进入清理或终态后必须释放可操作的视口绑定。
///
/// @author kongweiguang
#[test]
fn only_recoverable_ended_lifecycles_retain_terminal_history_binding() {
    assert!(lifecycle_retains_terminal_history(SessionLifecycle::Exited));
    assert!(lifecycle_retains_terminal_history(SessionLifecycle::Failed));
    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Running,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
    ] {
        assert!(!lifecycle_retains_terminal_history(lifecycle));
    }
}
