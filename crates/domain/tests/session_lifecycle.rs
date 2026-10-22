//! 会话生命周期领域规则的外部契约测试。
//!
//! @author kongweiguang

use kerminal_domain::sessions::{SessionLifecycle, SessionLifecycleEvent};

/// 校验退出后必须经过重连状态，且只有清理成功后才进入终态。
#[test]
fn reconnect_and_close_follow_explicit_transitions() {
    let exited = SessionLifecycle::Running
        .transition(SessionLifecycleEvent::TransportExited)
        .expect("running transport may exit");
    let reconnecting = exited
        .transition(SessionLifecycleEvent::ReconnectStarted)
        .expect("exited session may reconnect");
    let running = reconnecting
        .transition(SessionLifecycleEvent::ReconnectSucceeded)
        .expect("successful reconnect returns to running");
    let closing = running
        .transition(SessionLifecycleEvent::CloseRequested)
        .expect("running session may close");
    let closed = closing
        .transition(SessionLifecycleEvent::CloseSucceeded)
        .expect("successful cleanup closes the session");

    assert_eq!(reconnecting, SessionLifecycle::Starting);
    assert_eq!(running, SessionLifecycle::Running);
    assert_eq!(closed, SessionLifecycle::Closed);
}

/// 验证关闭请求可安全重试，并且失败重连仍可由关闭流程回收。
#[test]
fn close_request_is_idempotent_and_failed_reconnect_can_close() {
    let closing = SessionLifecycle::Starting
        .transition(SessionLifecycleEvent::CloseRequested)
        .expect("starting session may be cancelled by close");
    assert_eq!(
        closing
            .transition(SessionLifecycleEvent::CloseRequested)
            .expect("cleanup retry keeps the session closing"),
        SessionLifecycle::Closing
    );

    let failed = SessionLifecycle::Failed
        .transition(SessionLifecycleEvent::ReconnectStarted)
        .expect("failed session may reconnect")
        .transition(SessionLifecycleEvent::ReconnectFailed)
        .expect("failed reconnect remains recoverable");
    let closed = failed
        .transition(SessionLifecycleEvent::CloseRequested)
        .expect("failed session may close")
        .transition(SessionLifecycleEvent::CloseSucceeded)
        .expect("cleanup completes after reconnect failure");

    assert_eq!(failed, SessionLifecycle::Failed);
    assert_eq!(closed, SessionLifecycle::Closed);
}

/// Close 失败只恢复明确捕获的 Exited/Failed 历史态，不能使运行中或已关闭 session 复活。
///
/// @author kongweiguang
#[test]
fn close_failure_restores_only_captured_ended_lifecycle() {
    for previous in [SessionLifecycle::Exited, SessionLifecycle::Failed] {
        assert_eq!(
            SessionLifecycle::Closing
                .transition(SessionLifecycleEvent::CloseFailedToHistory { previous })
                .expect("only captured ended history returns to its original lifecycle"),
            previous
        );
    }

    for previous in [
        SessionLifecycle::Starting,
        SessionLifecycle::Running,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
    ] {
        assert!(
            SessionLifecycle::Closing
                .transition(SessionLifecycleEvent::CloseFailedToHistory { previous })
                .is_err(),
            "non-ended lifecycle must not be restored after close failure"
        );
    }
    assert!(
        SessionLifecycle::Closed
            .transition(SessionLifecycleEvent::CloseFailedToHistory {
                previous: SessionLifecycle::Exited,
            })
            .is_err()
    );
}

/// 拒绝跳过重连或从 Closing 直接恢复，避免未完成清理的 transport 复活。
#[test]
fn reconnect_requires_an_exited_or_failed_session() {
    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Running,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
    ] {
        assert!(
            lifecycle
                .transition(SessionLifecycleEvent::ReconnectStarted)
                .is_err()
        );
    }
}

/// 显式重启接受运行中或断开态，并拒绝启动中、关闭中和已关闭的 session。
///
/// @author kongweiguang
#[test]
fn restart_started_accepts_running_or_disconnected_sessions() {
    for lifecycle in [
        SessionLifecycle::Running,
        SessionLifecycle::Exited,
        SessionLifecycle::Failed,
    ] {
        assert_eq!(
            lifecycle
                .transition(SessionLifecycleEvent::RestartStarted)
                .expect("running or disconnected session may restart"),
            SessionLifecycle::Starting
        );
    }

    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
    ] {
        assert!(
            lifecycle
                .transition(SessionLifecycleEvent::RestartStarted)
                .is_err()
        );
    }
}

/// 迟到的 transport 回执不得覆盖关闭中的状态或终态。
///
/// @author kongweiguang
#[test]
fn late_transport_events_cannot_override_closing_or_closed() {
    let closing = SessionLifecycle::Running
        .transition(SessionLifecycleEvent::CloseRequested)
        .expect("running session may close");
    for event in [
        SessionLifecycleEvent::TransportExited,
        SessionLifecycleEvent::TransportFailed,
    ] {
        assert!(closing.transition(event).is_err());
    }

    let closed = closing
        .transition(SessionLifecycleEvent::CloseSucceeded)
        .expect("closing session may finish cleanup");
    for event in [
        SessionLifecycleEvent::CloseRequested,
        SessionLifecycleEvent::CloseSucceeded,
        SessionLifecycleEvent::TransportExited,
        SessionLifecycleEvent::TransportFailed,
        SessionLifecycleEvent::ReconnectStarted,
        SessionLifecycleEvent::RestartStarted,
        SessionLifecycleEvent::ReconnectSucceeded,
        SessionLifecycleEvent::ReconnectFailed,
    ] {
        let error = closed.transition(event).expect_err("closed is terminal");
        assert_eq!(error.current, SessionLifecycle::Closed);
        assert_eq!(error.event, event);
    }
}

/// 确认传输失败和重连失败采用不同事件但都进入可回收的 Failed 状态。
#[test]
fn transport_failure_and_reconnect_failure_are_recoverable() {
    let transport_failed = SessionLifecycle::Running
        .transition(SessionLifecycleEvent::TransportFailed)
        .expect("running transport may fail");
    let reconnect_failed = SessionLifecycle::Exited
        .transition(SessionLifecycleEvent::ReconnectStarted)
        .expect("exited session may reconnect")
        .transition(SessionLifecycleEvent::ReconnectFailed)
        .expect("reconnect failure is represented explicitly");

    assert_eq!(transport_failed, SessionLifecycle::Failed);
    assert_eq!(reconnect_failed, SessionLifecycle::Failed);
}
