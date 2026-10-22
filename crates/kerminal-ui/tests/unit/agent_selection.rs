//! Agent 外部导航选择的状态准入回归。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString};

use kerminal_application::agents::{AgentResumeCapability, AgentSessionView};
use kerminal_domain::{
    agents::{AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionStatus},
    identity::{AgentSessionId, SessionId},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};

use super::{AgentSelectionError, new_terminal_focus_target, validate_existing_agent_selection};

/// 使用完整业务模型构造列表项；导航测试不推断 provider 恢复能力，默认保持未核实。
///
/// @author kongweiguang
fn agent_view(id: AgentSessionId, status: AgentSessionStatus) -> AgentSessionView {
    let mut session = AgentSession::new(
        id,
        AgentProvider::Codex,
        None,
        "Review agent".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "workspace".to_owned(),
        "workspace/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "workspace/agents/session".to_owned(),
        },
        None,
        None,
    )
    .expect("valid Agent session fixture");
    session.status = status;
    AgentSessionView {
        session,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
        terminal: None,
        resume_recovery: None,
    }
}

/// 只接受最近一次成功列表中的精确 id，且不把归档记录当作可激活会话。
///
/// @author kongweiguang
#[test]
fn existing_selection_requires_verified_non_archived_record() {
    let active_id = AgentSessionId::new();
    let stale_id = AgentSessionId::new();
    let archived_id = AgentSessionId::new();
    let sessions = vec![
        agent_view(active_id.clone(), AgentSessionStatus::Active),
        agent_view(stale_id.clone(), AgentSessionStatus::Stale),
        agent_view(archived_id.clone(), AgentSessionStatus::Archived),
    ];

    assert_eq!(
        validate_existing_agent_selection(&sessions, false, true, &active_id),
        Ok(())
    );
    assert_eq!(
        validate_existing_agent_selection(&sessions, false, true, &stale_id),
        Ok(())
    );
    assert_eq!(
        validate_existing_agent_selection(&sessions, false, true, &archived_id),
        Err(AgentSelectionError::Archived)
    );
}

/// 刷新中、列表读取失败和 id 缺失都显式拒绝，调用方不会得到隐式替代会话。
///
/// @author kongweiguang
#[test]
fn existing_selection_rejects_uncertain_or_missing_list_entries() {
    let requested_id = AgentSessionId::new();
    let sessions = vec![agent_view(requested_id.clone(), AgentSessionStatus::Active)];
    let other_id = AgentSessionId::new();

    assert_eq!(
        validate_existing_agent_selection(&sessions, true, true, &requested_id),
        Err(AgentSelectionError::ListRefreshing)
    );
    assert_eq!(
        validate_existing_agent_selection(&sessions, false, false, &requested_id),
        Err(AgentSelectionError::ListUnavailable)
    );
    assert_eq!(
        validate_existing_agent_selection(&sessions, false, true, &other_id),
        Err(AgentSelectionError::NotFound)
    );
}

/// 自动焦点只接受创建回执中的同 Agent、Running PTY；Exited、错 owner、归档及普通刷新投影都不合格。
///
/// @author kongweiguang
#[test]
fn new_terminal_focus_requires_exact_running_agent_owner() {
    let agent_id = AgentSessionId::new();
    let terminal_id = SessionId::new();
    let mut view = agent_view(agent_id.clone(), AgentSessionStatus::Active);
    view.terminal = Some(SessionInfo {
        session_id: terminal_id.clone(),
        owner: SessionOwner::Agent {
            agent_session_id: agent_id.clone(),
        },
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("local-test-echo"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    });

    assert_eq!(
        new_terminal_focus_target(&view),
        Some((agent_id.clone(), terminal_id))
    );
    assert_eq!(
        new_terminal_focus_target(&agent_view(agent_id.clone(), AgentSessionStatus::Active,)),
        None
    );

    let mut exited = view.clone();
    exited.terminal.as_mut().unwrap().lifecycle = SessionLifecycle::Exited;
    assert_eq!(new_terminal_focus_target(&exited), None);

    let mut wrong_owner = view.clone();
    wrong_owner.terminal.as_mut().unwrap().owner = SessionOwner::Agent {
        agent_session_id: AgentSessionId::new(),
    };
    assert_eq!(new_terminal_focus_target(&wrong_owner), None);

    let mut archived = view;
    archived.session.status = AgentSessionStatus::Archived;
    assert_eq!(new_terminal_focus_target(&archived), None);
}
