//! 宿主退出失败的持久提示、准备阶段隔离与安全关闭策略回归。
//!
//! @author kongweiguang

use gpui_kit::SharedString;
use kerminal_application::{platform::window_state::ClosePolicy, updates::DownloadedUpdateId};

use super::{
    RootHostLifecycle, UpdateInstallRequest, can_retry_exit, host_shutdown_presentation,
    quit_preparation_is_running, retryable_update_request, window_close_policy,
    workspace_quit_preparation_failure, workspace_quit_preparation_in_flight,
};

/// Stopping 也包含 CAS 准备阶段，未派发收尾前不得遮住确认弹层或取消后恢复的工作台。
///
/// @author kongweiguang
#[test]
fn preparing_or_cancelled_quit_keeps_the_normal_workbench() {
    for lifecycle in [RootHostLifecycle::Running, RootHostLifecycle::Stopping] {
        assert!(host_shutdown_presentation(false, lifecycle, None, 0).is_none());
    }
    assert_eq!(
        window_close_policy(false, true, false),
        ClosePolicy::HideToTray
    );
    assert_eq!(
        window_close_policy(false, false, false),
        ClosePolicy::RequestQuit
    );
}

/// 原错不取自普通 status；重试期间继续显示它，但只有失败终态才开放下一次退出请求。
///
/// @author kongweiguang
#[test]
fn host_failure_survives_retry_and_waiting_does_not_enable_another_retry() {
    let failure = SharedString::from("固定宿主收尾失败样本");
    let failed = host_shutdown_presentation(true, RootHostLifecycle::StopFailed, Some(&failure), 0)
        .expect("committed failure must stay visible");
    assert_eq!(failed.message, failure);
    assert_eq!(failed.managed_tasks, 0);
    assert!(!failed.cleanup_note.is_empty());
    assert!(failed.retry_exit);

    let retrying = host_shutdown_presentation(true, RootHostLifecycle::Stopping, Some(&failure), 3)
        .expect("an accepted retry must keep the stopping projection");
    assert_eq!(retrying.message, failure);
    assert_eq!(retrying.managed_tasks, 3);
    assert!(!retrying.retry_exit, "do not enqueue a parallel retry");
}

/// 所有现有失败终态均能显式退出，结果未知仍保持停止投影而非重新使用普通服务。
///
/// @author kongweiguang
#[test]
fn every_failed_host_terminal_state_can_retry_exit_without_becoming_usable() {
    for lifecycle in [
        RootHostLifecycle::StopFailed,
        RootHostLifecycle::StoppedUpdateFailed,
        RootHostLifecycle::OutcomeUnknown,
        RootHostLifecycle::RestartFailed,
        RootHostLifecycle::ListenerRecoveryFailed,
    ] {
        assert!(can_retry_exit(lifecycle));
        let projection = host_shutdown_presentation(true, lifecycle, None, 0)
            .expect("failure cannot restore the ordinary workbench");
        assert!(projection.retry_exit);
        assert!(!projection.message.is_empty());
        assert_eq!(
            window_close_policy(true, true, false),
            ClosePolicy::RequestQuit
        );
        assert_eq!(
            window_close_policy(true, false, false),
            ClosePolicy::RequestQuit
        );
    }
    for lifecycle in [
        RootHostLifecycle::Running,
        RootHostLifecycle::Stopping,
        RootHostLifecycle::Stopped,
    ] {
        assert!(!can_retry_exit(lifecycle));
    }
}

/// 已派发事实不可由生命周期误写 Running 清除，保护部分停止服务不会被投影成可继续工作。
///
/// @author kongweiguang
#[test]
fn a_committed_shutdown_never_projects_a_usable_workbench() {
    let projection = host_shutdown_presentation(true, RootHostLifecycle::Running, None, 0)
        .expect("the submitted shutdown is the authoritative irreversible boundary");
    assert!(!projection.retry_exit);
    assert_eq!(
        window_close_policy(true, true, false),
        ClosePolicy::RequestQuit
    );
}

/// executor intake 关闭只显示受管任务数，不以任务归零代替 host 的会话和 listener 清理回执。
///
/// @author kongweiguang
#[test]
fn executor_closing_keeps_waiting_for_host_cleanup_even_with_active_tasks() {
    let projection = host_shutdown_presentation(true, RootHostLifecycle::Stopping, None, 2)
        .expect("executor closing must immediately replace the live workbench");

    assert_eq!(projection.managed_tasks, 2);
    assert!(!projection.retry_exit);
    assert!(!projection.cleanup_note.is_empty());
}

/// 更新重试仅保留原确定失败的句柄，未知结果、启动失败和监听恢复失败均不能重放安装。
///
/// @author kongweiguang
#[test]
fn retry_update_retains_exact_identity_and_rejects_unknown_terminal_states() {
    let request = UpdateInstallRequest {
        update_id: DownloadedUpdateId::new(),
        version: "fixture-version".to_owned(),
    };
    for lifecycle in [
        RootHostLifecycle::StopFailed,
        RootHostLifecycle::StoppedUpdateFailed,
    ] {
        assert_eq!(
            retryable_update_request(lifecycle, Some(&request)),
            Some(&request)
        );
        assert!(retryable_update_request(lifecycle, None).is_none());
    }
    for lifecycle in [
        RootHostLifecycle::Running,
        RootHostLifecycle::Stopping,
        RootHostLifecycle::OutcomeUnknown,
        RootHostLifecycle::RestartFailed,
        RootHostLifecycle::ListenerRecoveryFailed,
        RootHostLifecycle::Stopped,
    ] {
        assert!(retryable_update_request(lifecycle, Some(&request)).is_none());
    }
}

/// 模拟布局收据先于原 workspace CAS 返回；纯阶段门禁必须保留退出意图，不能派发宿主事件。
///
/// @author kongweiguang
#[test]
fn late_layout_receipt_waits_for_original_workspace_cas() {
    let mut lifecycle = RootHostLifecycle::Stopping;
    let mut pending = Some(super::QuitIntent::Exit);
    let mut resumed = 0;
    let mut emitted = 0;
    if quit_preparation_is_running(lifecycle, false) && pending.is_some() {
        resumed += 1;
    }
    // 编辑器/Preferences 的迟到入口也复用 emit 前的同一 CAS fence。
    if !workspace_quit_preparation_in_flight(lifecycle, false) {
        let _ = pending.take();
        emitted += 1;
    }
    assert_eq!(resumed, 0);
    assert_eq!(emitted, 0);
    assert_eq!(pending, Some(super::QuitIntent::Exit));

    // 只有原 CAS 回执可结束 Stopping；成功后才允许重新通过普通准备门禁。
    lifecycle = RootHostLifecycle::Running;
    assert!(quit_preparation_is_running(lifecycle, false));
    assert!(!workspace_quit_preparation_in_flight(lifecycle, false));
    assert!(!quit_preparation_is_running(lifecycle, true));
}

/// 普通回执只属于 Running/未派发，CAS fence 不改变已经提交的失败重试或收尾事实。
///
/// @author kongweiguang
#[test]
fn quit_receipt_phase_policy_covers_every_existing_lifecycle() {
    for lifecycle in [
        RootHostLifecycle::Running,
        RootHostLifecycle::Stopping,
        RootHostLifecycle::StopFailed,
        RootHostLifecycle::StoppedUpdateFailed,
        RootHostLifecycle::OutcomeUnknown,
        RootHostLifecycle::RestartFailed,
        RootHostLifecycle::ListenerRecoveryFailed,
        RootHostLifecycle::Stopped,
    ] {
        for requested in [false, true] {
            assert_eq!(
                quit_preparation_is_running(lifecycle, requested),
                lifecycle == RootHostLifecycle::Running && !requested,
            );
            assert_eq!(
                workspace_quit_preparation_in_flight(lifecycle, requested),
                lifecycle == RootHostLifecycle::Stopping && !requested,
            );
        }
    }
}

/// Busy 和保存失败保持可用工作台及原错，X 必须重试退出，不能静默隐藏或冒充 host 已提交。
///
/// @author kongweiguang
#[test]
fn blocked_workspace_quit_keeps_failure_visible_and_prevents_tray_hide() {
    use super::terminal_open::WorkspaceSnapshotSaveCompletion;
    for result in [
        WorkspaceSnapshotSaveCompletion::Busy,
        WorkspaceSnapshotSaveCompletion::Conflict,
        WorkspaceSnapshotSaveCompletion::ReadbackConfirmedUncommitted,
        WorkspaceSnapshotSaveCompletion::OutcomeUnknown,
        WorkspaceSnapshotSaveCompletion::Failed("保存失败样本".into()),
    ] {
        let failure = workspace_quit_preparation_failure(&result)
            .expect("rejected workspace preparation must remain visible");
        assert!(!failure.is_empty());
        assert!(host_shutdown_presentation(false, RootHostLifecycle::Running, None, 0).is_none());
        for tray_installed in [false, true] {
            assert_eq!(
                window_close_policy(false, tray_installed, true),
                ClosePolicy::RequestQuit,
            );
        }
    }
    assert_eq!(
        workspace_quit_preparation_failure(&WorkspaceSnapshotSaveCompletion::Failed(
            "保留精确原错".into(),
        )),
        Some(SharedString::from("保留精确原错")),
    );
}

/// 成功准备没有失败提示；用户取消后恢复普通托盘策略，已提交的退出仍不能隐藏。
///
/// @author kongweiguang
#[test]
fn successful_or_cancelled_preparation_restores_normal_close_policy() {
    use super::terminal_open::WorkspaceSnapshotSaveCompletion;
    for result in [
        WorkspaceSnapshotSaveCompletion::Saved,
        WorkspaceSnapshotSaveCompletion::NoChanges,
    ] {
        assert!(workspace_quit_preparation_failure(&result).is_none());
    }
    assert_eq!(
        window_close_policy(false, true, false),
        ClosePolicy::HideToTray
    );
    assert_eq!(
        window_close_policy(false, false, false),
        ClosePolicy::RequestQuit
    );
    assert_eq!(
        window_close_policy(true, true, false),
        ClosePolicy::RequestQuit
    );
    assert_eq!(
        window_close_policy(true, true, true),
        ClosePolicy::RequestQuit
    );
}
