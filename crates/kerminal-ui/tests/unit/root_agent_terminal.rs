//! Agent 终端首帧的主题代次与 watcher 所有权回归。
//!
//! @author kongweiguang

use super::*;

/// 用户切换 compact 工具或主机时即使共享句柄未变，迟到 Ready 也不能越过已推进的导航代次。
///
/// @author kongweiguang
#[test]
fn creation_focus_rejects_compact_navigation_with_unchanged_shared_handle() {
    assert!(agent_creation_focus_source_is_current(7, 7, true));
    assert!(!agent_creation_focus_source_is_current(7, 8, true));
    assert!(!agent_creation_focus_source_is_current(7, 7, false));
    assert!(!agent_creation_focus_source_is_current(7, 8, false));
}

/// Agent 终态只把仍由精确旧 PTY 占有的焦点交回父宿主；其它导航、模态和非终态均不抢焦点。
///
/// @author kongweiguang
#[test]
fn exited_agent_returns_focus_only_to_the_current_parent_surface() {
    assert_eq!(
        agent_terminal_exit_focus_return_target(SessionLifecycle::Exited, true, true, true, true,),
        Some(AgentTerminalExitFocusReturnTarget::CompactDrawer)
    );
    assert_eq!(
        agent_terminal_exit_focus_return_target(SessionLifecycle::Closed, true, true, true, false,),
        Some(AgentTerminalExitFocusReturnTarget::Workspace)
    );
    assert_eq!(
        agent_terminal_exit_focus_return_target(SessionLifecycle::Failed, true, true, true, false,),
        Some(AgentTerminalExitFocusReturnTarget::Workspace)
    );
    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Running,
        SessionLifecycle::Closing,
    ] {
        assert_eq!(
            agent_terminal_exit_focus_return_target(lifecycle, true, true, true, true),
            None
        );
    }
    assert_eq!(
        agent_terminal_exit_focus_return_target(SessionLifecycle::Exited, false, true, true, true),
        None
    );
    assert_eq!(
        agent_terminal_exit_focus_return_target(SessionLifecycle::Exited, true, false, true, true),
        None
    );
    assert_eq!(
        agent_terminal_exit_focus_return_target(SessionLifecycle::Exited, true, true, false, true),
        None
    );
}

/// 关闭竞态核对仅接受同 owner/PTY 的 Exited；运行冲突、其它 Agent 及旧会话不能被吞掉。
///
/// @author kongweiguang
#[test]
fn input_context_conflict_needs_exact_agent_and_terminal_exit() {
    use kerminal_domain::sessions::{LocalSessionTarget, SessionTarget, TerminalSize};
    use std::{collections::BTreeMap, ffi::OsString};

    let session_id = SessionId::new();
    let owner = SessionOwner::Agent {
        agent_session_id: AgentSessionId::new(),
    };
    let mut session = SessionInfo {
        session_id: session_id.clone(),
        owner: owner.clone(),
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("local-test-echo"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    };
    assert!(!agent_context_conflict_is_confirmed_exit(
        std::slice::from_ref(&session),
        &owner,
        &session_id
    ));
    session.lifecycle = SessionLifecycle::Exited;
    assert!(agent_context_conflict_is_confirmed_exit(
        std::slice::from_ref(&session),
        &owner,
        &session_id
    ));
    assert!(!agent_context_conflict_is_confirmed_exit(
        std::slice::from_ref(&session),
        &owner,
        &SessionId::new()
    ));
    session.owner = SessionOwner::Agent {
        agent_session_id: AgentSessionId::new(),
    };
    assert!(!agent_context_conflict_is_confirmed_exit(
        std::slice::from_ref(&session),
        &owner,
        &session_id
    ));
    assert!(!agent_context_conflict_is_confirmed_exit(
        &[],
        &owner,
        &session_id
    ));
}

/// Root 只允许与请求一致的 Agent/PTY 固定 binding 消费焦点意图，不能转投其它会话或 owner。
///
/// @author kongweiguang
#[test]
fn creation_focus_binding_requires_exact_agent_and_terminal() {
    let agent_session_id = AgentSessionId::new();
    let session_id = SessionId::new();
    let target = (agent_session_id.clone(), session_id.clone());
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: SessionOwner::Agent {
            agent_session_id: agent_session_id.clone(),
        },
        session_generation: 4,
    };

    assert!(agent_creation_focus_binding_matches(
        &target,
        &agent_session_id,
        &session_id,
        &binding
    ));
    assert!(!agent_creation_focus_binding_matches(
        &target,
        &AgentSessionId::new(),
        &session_id,
        &binding
    ));
    assert!(!agent_creation_focus_binding_matches(
        &target,
        &agent_session_id,
        &SessionId::new(),
        &binding
    ));

    let wrong_owner = UiSessionBinding {
        owner: SessionOwner::Agent {
            agent_session_id: AgentSessionId::new(),
        },
        ..binding
    };
    assert!(!agent_creation_focus_binding_matches(
        &target,
        &agent_session_id,
        &session_id,
        &wrong_owner
    ));
}

/// 异步创建期间的重复旧选择投影不丢焦点意图；用户切换或就绪后选错目标必须撤销。
///
/// @author kongweiguang
#[test]
fn creation_focus_ignores_replayed_baseline_selection_but_not_user_switches() {
    let baseline_agent = AgentSessionId::new();
    let baseline_terminal = SessionId::new();
    let intent = PendingAgentFocusIntent {
        request_generation: 7,
        pane_focus_generation: 3,
        expected_focus: None,
        selected_agent_at_start: Some(baseline_agent.clone()),
        selected_terminal_at_start: Some(baseline_terminal.clone()),
        target: None,
    };

    assert!(agent_creation_selection_matches(
        &intent,
        Some(&baseline_agent),
        Some(&baseline_terminal)
    ));
    assert!(!agent_creation_selection_matches(
        &intent,
        Some(&AgentSessionId::new()),
        Some(&baseline_terminal)
    ));

    let target_agent = AgentSessionId::new();
    let target_terminal = SessionId::new();
    let ready_intent = PendingAgentFocusIntent {
        target: Some((target_agent.clone(), target_terminal.clone())),
        ..intent
    };
    assert!(agent_creation_selection_matches(
        &ready_intent,
        Some(&target_agent),
        Some(&target_terminal)
    ));
    assert!(!agent_creation_selection_matches(
        &ready_intent,
        Some(&baseline_agent),
        Some(&baseline_terminal)
    ));
}

/// 首帧同步配色；稳定主题下连续输出帧只同步一次，主题 revision 改变后再同步一次。
///
/// @author kongweiguang
#[test]
fn stable_output_frames_sync_palette_once_and_new_revision_resyncs() {
    assert!(agent_palette_update_required(None, 0));
    assert!(!agent_palette_update_required(Some(0), 0));
    assert!(agent_palette_update_required(Some(0), 1));

    let mut applied_generation = None;
    let mut writes = 0;
    for _ in 0..4 {
        if agent_palette_update_required(applied_generation, 3) {
            writes += 1;
            applied_generation = Some(3);
        }
    }
    assert_eq!(writes, 1);
}

/// 读取期间主题变更时旧代次快照不得提交，使用新代次重新读取后才允许进入面板。
///
/// @author kongweiguang
#[test]
fn palette_change_during_frame_read_rejects_stale_snapshot_and_accepts_retry() {
    assert_eq!(
        agent_frame_acceptance(true, 4, 5, Some(true)),
        AgentFrameAcceptance::RetryWithCurrentPalette
    );
    assert_eq!(
        agent_frame_acceptance(true, 5, 5, Some(true)),
        AgentFrameAcceptance::Accept
    );
}

/// 即使 Root 代次未变，快照中仍是旧 palette 时也必须重写并重读，防止并行旧更新覆写引擎。
///
/// @author kongweiguang
#[test]
fn snapshot_with_stale_palette_retries_even_when_root_generation_matches() {
    assert_eq!(
        agent_frame_acceptance(true, 8, 8, Some(false)),
        AgentFrameAcceptance::RetryWithCurrentPalette
    );
}

/// Agent 或 PTY 选择代次切换后，即使配色相同也禁止旧 watcher 发布其迟到帧。
///
/// @author kongweiguang
#[test]
fn changed_watch_owner_stops_before_palette_comparison() {
    assert_eq!(
        agent_frame_acceptance(false, 4, 4, None),
        AgentFrameAcceptance::StopStaleWatch
    );
    assert_eq!(
        agent_frame_acceptance(false, 4, 5, Some(false)),
        AgentFrameAcceptance::StopStaleWatch
    );
}

/// 主题与 watcher 均未变化时，读取到的快照可进入 Agent 面板投影。
///
/// @author kongweiguang
#[test]
fn matching_watch_and_palette_revision_accept_frame() {
    assert!(agent_lifecycle_allows_snapshot(SessionLifecycle::Running));
    assert!(agent_lifecycle_allows_input(SessionLifecycle::Running));
    assert_eq!(
        agent_frame_acceptance(true, 7, 7, Some(true)),
        AgentFrameAcceptance::Accept
    );
}

/// Exited 只允许刷新已保存画布；输入仍停用，避免恢复时重建 PTY 命令上下文。
///
/// @author kongweiguang
#[test]
fn exited_keeps_snapshot_refresh_but_never_restores_input() {
    assert!(agent_lifecycle_allows_snapshot(SessionLifecycle::Exited));
    assert!(!agent_lifecycle_allows_input(SessionLifecycle::Exited));
    assert_eq!(
        agent_frame_acceptance(true, 9, 9, Some(true)),
        AgentFrameAcceptance::Accept
    );
}

/// 只有 Running 可交互；Closed、Failed 与过渡态都不刷新画布或重新建立输入。
///
/// @author kongweiguang
#[test]
fn non_running_non_exited_lifecycles_stop_snapshot_and_input() {
    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
        SessionLifecycle::Failed,
    ] {
        assert!(!agent_lifecycle_allows_snapshot(lifecycle));
        assert!(!agent_lifecycle_allows_input(lifecycle));
    }
}
