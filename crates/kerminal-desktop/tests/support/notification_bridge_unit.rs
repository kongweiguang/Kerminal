//! 通知桥只覆盖旧策略支持的 SFTP 终态，不把容器或用户取消误报为结果。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

use kerminal_application::agents::{
    AgentResumeCapability, AgentResumeRecovery, AgentResumeRecoveryStatus, AgentSessionView,
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionScope, AgentSessionStatus,
    },
    files::{
        ConflictPolicy, FileEndpoint, TransferCommitSummary, TransferKind, TransferRequest,
        TransferStatus, TransferSummary,
    },
    identity::{AgentSessionId, HostId, SessionId, TransferId},
    sessions::{
        LocalSessionTarget, SessionExitStatus, SessionInfo, SessionLifecycle, SessionOwner,
        SessionTarget, TerminalSize,
    },
};
use tokio::time::Instant;

use super::{
    AgentCompletionIdentity, AgentNotificationTracker, agent_notification_for, notification_for,
    sftp_host_ids,
};

/// 构造带真实运行时 owner 的 Agent 终端快照，供通知边界测试区分退出和关闭。
///
/// @author kongweiguang
fn agent_terminal(
    agent_session_id: AgentSessionId,
    session_id: SessionId,
    lifecycle: SessionLifecycle,
) -> SessionInfo {
    let exit_status = (lifecycle == SessionLifecycle::Exited).then_some(SessionExitStatus {
        code: Some(23),
        signal: None,
        success: false,
    });
    SessionInfo {
        session_id,
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("codex"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Agent { agent_session_id },
        size: TerminalSize::default(),
        lifecycle,
        exit_status,
    }
}

/// 构造满足领域校验的 Agent view；通知测试不验证恢复元数据，因此使用保守的不可用投影。
///
/// @author kongweiguang
fn agent_view(
    agent_session_id: AgentSessionId,
    terminal: Option<SessionInfo>,
    title: &str,
    status: AgentSessionStatus,
) -> AgentSessionView {
    let launch = AgentSessionLaunch {
        command_label: "Codex".to_owned(),
        shell: "codex".to_owned(),
        args: Vec::new(),
        cwd: "C:\\workspace\\agent".to_owned(),
    };
    let session = AgentSession::restore(
        1,
        agent_session_id,
        AgentProvider::Codex,
        None,
        title.to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        status,
        "C:\\workspace".to_owned(),
        "C:\\workspace\\agent".to_owned(),
        launch,
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("valid Agent session fixture");
    AgentSessionView {
        session,
        terminal,
        resume_recovery: None,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
    }
}

/// 为通知投影测试构造完整不可变任务摘要，避免直接构造内部 TransferTask。
///
/// @author kongweiguang
fn summary(
    status: TransferStatus,
    source: FileEndpoint,
    destination: FileEndpoint,
) -> TransferSummary {
    TransferSummary {
        id: TransferId::new(),
        request: TransferRequest {
            source,
            destination,
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Overwrite,
            idle_timeout_seconds: None,
        },
        status,
        phase: None,
        bytes_transferred: 0,
        total_bytes: None,
        speed_bytes_per_second: 0,
        last_progress_at: None,
        effective_idle_timeout_seconds: None,
        retryable: false,
        resumable: false,
        failure_kind: None,
        safe_error: None,
        actual_destination: None,
        automatic_recovery_used: false,
        retry_successor: None,
        commit_summary: TransferCommitSummary::empty(),
        partial_commit: false,
        cleanup_pending: false,
    }
}

/// Remote 与 local 文件端点属于 SFTP 通知族；容器和纯本机操作不属于该策略。
#[test]
fn recognizes_only_remote_file_transfers_for_sftp_notifications() {
    let host_id = HostId::parse("saved-host").expect("valid host identifier");
    let remote =
        FileEndpoint::remote(host_id.clone(), "/data/report.txt").expect("valid remote path");
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work\\report.txt"),
    };
    let sftp = summary(TransferStatus::Succeeded, local.clone(), remote);
    assert_eq!(sftp_host_ids(&sftp), Some(vec![host_id]));

    let container = FileEndpoint::Container {
        host_id: None,
        runtime: kerminal_domain::files::ContainerRuntime::Docker,
        container_id: "container-1".to_owned(),
        path: kerminal_domain::files::RemotePath::parse("/tmp/report.txt")
            .expect("valid container path"),
    };
    assert!(sftp_host_ids(&summary(TransferStatus::Failed, local.clone(), container)).is_none());
    assert!(sftp_host_ids(&summary(TransferStatus::Succeeded, local.clone(), local)).is_none());
}

/// 成功/失败映射为已有通知类型，取消、跳过和无变化不会生成系统提示。
#[test]
fn maps_only_supported_final_outcomes() {
    let host_id = HostId::parse("saved-host").expect("valid host identifier");
    let remote = FileEndpoint::remote(host_id, "/data/report.txt").expect("valid remote path");
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work\\report.txt"),
    };
    let duration = Duration::from_secs(20);
    let success = summary(TransferStatus::Succeeded, local.clone(), remote.clone());
    let failure = summary(TransferStatus::Failed, local.clone(), remote.clone());
    let cancelled = summary(TransferStatus::Cancelled, local, remote);

    let success_event = notification_for(&success, duration, Some("Build host".to_owned()))
        .expect("successful SFTP transfer notification");
    assert!(matches!(
        success_event,
        kerminal_application::platform::DesktopNotificationEvent::SftpBatchCompleted {
            total_count: 1,
            succeeded_count: Some(1),
            failed_count: Some(0),
            duration: event_duration,
            ..
        } if event_duration == duration
    ));

    let failure_event = notification_for(&failure, Duration::ZERO, None)
        .expect("failed SFTP transfer notification");
    assert!(matches!(
        failure_event,
        kerminal_application::platform::DesktopNotificationEvent::SftpTransferFailed {
            failed_count: 1,
            ..
        }
    ));
    assert!(notification_for(&cancelled, Duration::ZERO, None).is_none());
}

/// 仅匹配 Active Agent 拥有的自然 Exited runtime，且使用其真实退出码和脱控制字符标题。
///
/// @author kongweiguang
#[test]
fn maps_only_natural_agent_exit_to_agent_finished() {
    let agent_id = AgentSessionId::new();
    let session_id = SessionId::new();
    let terminal = agent_terminal(
        agent_id.clone(),
        session_id.clone(),
        SessionLifecycle::Exited,
    );
    let view = agent_view(
        agent_id.clone(),
        Some(terminal.clone()),
        "Build\rAgent",
        AgentSessionStatus::Active,
    );
    let duration = Duration::from_secs(9);
    let identity = AgentCompletionIdentity {
        agent_session_id: agent_id.clone(),
        session_id: session_id.clone(),
    };

    let event = agent_notification_for(&identity, &view, &terminal, duration)
        .expect("natural Agent CLI exit produces a notification");
    assert!(matches!(
        event,
        kerminal_application::platform::DesktopNotificationEvent::AgentFinished {
            agent_name,
            exit_code: Some(23),
            duration: event_duration,
            ..
        } if agent_name == "BuildAgent" && event_duration == duration
    ));

    let stale_after_natural_exit = agent_view(
        agent_id.clone(),
        Some(terminal.clone()),
        "Build Agent",
        AgentSessionStatus::Stale,
    );
    assert!(
        agent_notification_for(&identity, &stale_after_natural_exit, &terminal, duration).is_some()
    );

    let resumed_session_id = SessionId::new();
    let resumed_on_another_terminal = agent_view(
        agent_id.clone(),
        Some(agent_terminal(
            agent_id.clone(),
            resumed_session_id,
            SessionLifecycle::Running,
        )),
        "Build Agent",
        AgentSessionStatus::Active,
    );
    assert!(
        agent_notification_for(&identity, &resumed_on_another_terminal, &terminal, duration)
            .is_none(),
        "an old exited CLI must not notify after this Agent starts a new runtime"
    );

    let mut archived = view.clone();
    archived.session.status = AgentSessionStatus::Archived;
    assert!(agent_notification_for(&identity, &archived, &terminal, duration).is_none());

    let mut manually_closed = terminal.clone();
    manually_closed.lifecycle = SessionLifecycle::Closed;
    assert!(agent_notification_for(&identity, &view, &manually_closed, duration).is_none());

    let mut failed = terminal.clone();
    failed.lifecycle = SessionLifecycle::Failed;
    assert!(agent_notification_for(&identity, &view, &failed, duration).is_none());

    let mut missing_exit_result = terminal.clone();
    missing_exit_result.exit_status = None;
    assert!(agent_notification_for(&identity, &view, &missing_exit_result, duration).is_none());

    let mut mismatched_owner = terminal.clone();
    mismatched_owner.owner = SessionOwner::Agent {
        agent_session_id: AgentSessionId::new(),
    };
    assert!(agent_notification_for(&identity, &view, &mismatched_owner, duration).is_none());

    let mut mismatched_session = terminal.clone();
    mismatched_session.session_id = SessionId::new();
    assert!(agent_notification_for(&identity, &view, &mismatched_session, duration).is_none());

    let mut pending_recovery = view.clone();
    pending_recovery.resume_recovery = Some(AgentResumeRecovery {
        terminal_session_id: session_id,
        status: AgentResumeRecoveryStatus::Pending,
    });
    assert!(agent_notification_for(&identity, &pending_recovery, &terminal, duration).is_none());
}

/// 启动时已有的退出只作为历史基线；运行期失败按同一身份退避并成功后只发送一次。
///
/// @author kongweiguang
#[test]
fn skips_historical_agent_exits_and_retries_with_bounded_identity_deduplication() {
    let agent_id = AgentSessionId::new();
    let session_id = SessionId::new();
    let now = Instant::now();
    let exited = agent_terminal(
        agent_id.clone(),
        session_id.clone(),
        SessionLifecycle::Exited,
    );

    let mut startup = AgentNotificationTracker::default();
    startup.establish_baseline(std::slice::from_ref(&exited), now);
    assert!(
        startup
            .observe_snapshot(std::slice::from_ref(&exited), now + Duration::from_secs(1))
            .is_empty()
    );

    let running = agent_terminal(
        agent_id.clone(),
        session_id.clone(),
        SessionLifecycle::Running,
    );
    let mut tracker = AgentNotificationTracker::default();
    assert!(
        tracker
            .observe_snapshot(std::slice::from_ref(&running), now)
            .is_empty()
    );
    let candidate = tracker
        .observe_snapshot(std::slice::from_ref(&exited), now + Duration::from_secs(2))
        .pop()
        .expect("runtime exit after observed run is eligible");
    assert_eq!(candidate.duration, Duration::from_secs(2));

    let retry_after = [1, 2, 4, 8, 16, 30, 30].into_iter().fold(
        now + Duration::from_secs(2),
        |failed_at, delay_seconds| {
            tracker.back_off_after_failure(&candidate.identity, failed_at);
            let retry_after = tracker.observations[&session_id].retry_after;
            assert_eq!(
                retry_after.duration_since(failed_at),
                Duration::from_secs(delay_seconds)
            );
            retry_after
        },
    );
    assert!(
        tracker
            .observe_snapshot(
                std::slice::from_ref(&exited),
                retry_after - Duration::from_millis(1),
            )
            .is_empty()
    );
    let retry = tracker
        .observe_snapshot(std::slice::from_ref(&exited), retry_after)
        .pop()
        .expect("same identity becomes eligible after backoff");
    assert_eq!(
        retry.identity,
        AgentCompletionIdentity {
            agent_session_id: agent_id,
            session_id: session_id.clone(),
        }
    );
    tracker.mark_processed(&retry.identity);
    assert!(
        tracker
            .observe_snapshot(&[exited], retry_after + Duration::from_secs(60))
            .is_empty()
    );
}
