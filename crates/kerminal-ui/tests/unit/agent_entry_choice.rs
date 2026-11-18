//! Agent 入口运行中会话匹配的回归：精确身份、真实运行 owner、完整列表。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString};

use kerminal_application::agents::{
    AgentResumeCapability, AgentResumeRecovery, AgentResumeRecoveryStatus, AgentSessionView,
};
use kerminal_domain::{
    agents::{
        AgentLauncherKey, AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionScope,
        AgentSessionStatus,
    },
    identity::{AgentSessionId, SessionId, TabId},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use tokio::runtime::Builder;

use super::entry::matching_running_sessions;
use super::test_support::FixtureAgentService;

/// 构造带精确 launcher、归属 scope 和业务时间的会话；测试不依赖真实配置或 CLI。
///
/// @author kongweiguang
fn agent_view(
    id: AgentSessionId,
    provider: AgentProvider,
    launcher_key: Option<AgentLauncherKey>,
    scope: AgentSessionScope,
    title: &str,
    updated_at: &str,
) -> AgentSessionView {
    let mut session = AgentSession::new(
        id,
        provider,
        launcher_key,
        title.to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "workspace".to_owned(),
        "workspace/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: provider_label(provider).to_owned(),
            shell: "agent".to_owned(),
            args: Vec::new(),
            cwd: "workspace/agents/session".to_owned(),
        },
        Some(scope),
        None,
    )
    .expect("valid Agent session fixture");
    session.updated_at = updated_at.to_owned();
    AgentSessionView {
        session,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
        terminal: None,
        resume_recovery: None,
    }
}

/// 给会话安装指定 owner 的 Running PTY；owner 可故意设为另一 Agent 以验证精确匹配。
///
/// @author kongweiguang
fn with_running_terminal(mut view: AgentSessionView, owner: AgentSessionId) -> AgentSessionView {
    view.terminal = Some(SessionInfo {
        session_id: SessionId::new(),
        owner: SessionOwner::Agent {
            agent_session_id: owner,
        },
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("agent"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    });
    view
}

/// Provider 只用于构造合法启动快照，不参与界面文案断言。
///
/// @author kongweiguang
fn provider_label(provider: AgentProvider) -> &'static str {
    match provider {
        AgentProvider::Codex => "Codex",
        AgentProvider::Claude => "Claude",
        AgentProvider::Pi => "PI Agent",
        AgentProvider::Custom => "Custom",
    }
}

/// 使用真实默认设置解析目录，确保测试与生产共享同一 launcher 身份集合。
///
/// @author kongweiguang
fn catalog() -> kerminal_application::agents::AgentLauncherCatalog {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let dummy = agent_view(
        AgentSessionId::new(),
        AgentProvider::Codex,
        None,
        AgentSessionScope::Global,
        "catalog",
        "2026-10-17T00:00:00Z",
    );
    runtime.block_on(FixtureAgentService::new(dummy)).catalog()
}

/// 入口只接受当前精确 launcher/provider/scope 且由该 Agent 自己持有的 Running PTY；
/// 已退出、待核对、归档和 owner 不符的记录都不能被当成“运行中”。
///
/// @author kongweiguang
#[test]
fn entry_matching_requires_exact_running_owner_and_scope() {
    let catalog = catalog();
    let codex_key = catalog
        .options
        .iter()
        .find(|option| option.provider == AgentProvider::Codex)
        .expect("default catalog has Codex")
        .key
        .clone();
    let claude_key = catalog
        .options
        .iter()
        .find(|option| option.provider == AgentProvider::Claude)
        .expect("default catalog has Claude")
        .key
        .clone();
    let scope = AgentSessionScope::Global;

    let running_id = AgentSessionId::new();
    let running = with_running_terminal(
        agent_view(
            running_id.clone(),
            AgentProvider::Codex,
            Some(codex_key.clone()),
            scope.clone(),
            "Running",
            "2026-10-17T01:00:00Z",
        ),
        running_id.clone(),
    );

    let exited_id = AgentSessionId::new();
    let mut exited = with_running_terminal(
        agent_view(
            exited_id.clone(),
            AgentProvider::Codex,
            Some(codex_key.clone()),
            scope.clone(),
            "Exited",
            "2026-10-17T02:00:00Z",
        ),
        exited_id,
    );
    exited.terminal.as_mut().unwrap().lifecycle = SessionLifecycle::Exited;

    let pending_id = AgentSessionId::new();
    let mut pending = with_running_terminal(
        agent_view(
            pending_id.clone(),
            AgentProvider::Codex,
            Some(codex_key.clone()),
            scope.clone(),
            "Pending",
            "2026-10-17T03:00:00Z",
        ),
        pending_id.clone(),
    );
    pending.resume_recovery = Some(AgentResumeRecovery {
        terminal_session_id: SessionId::new(),
        status: AgentResumeRecoveryStatus::Pending,
    });

    let wrong_owner_id = AgentSessionId::new();
    let wrong_owner = with_running_terminal(
        agent_view(
            wrong_owner_id,
            AgentProvider::Codex,
            Some(codex_key.clone()),
            scope.clone(),
            "Wrong owner",
            "2026-10-17T04:00:00Z",
        ),
        AgentSessionId::new(),
    );

    let tab_id = TabId::new();
    let other_scope_id = AgentSessionId::new();
    let other_scope = with_running_terminal(
        agent_view(
            other_scope_id.clone(),
            AgentProvider::Codex,
            Some(codex_key.clone()),
            AgentSessionScope::Tab {
                tab_id: tab_id.clone(),
            },
            "Other scope",
            "2026-10-17T05:00:00Z",
        ),
        other_scope_id,
    );

    let other_launcher_id = AgentSessionId::new();
    let other_launcher = with_running_terminal(
        agent_view(
            other_launcher_id,
            AgentProvider::Claude,
            Some(claude_key),
            scope.clone(),
            "Other launcher",
            "2026-10-17T06:00:00Z",
        ),
        AgentSessionId::new(),
    );

    let archived_id = AgentSessionId::new();
    let mut archived = with_running_terminal(
        agent_view(
            archived_id.clone(),
            AgentProvider::Codex,
            Some(codex_key.clone()),
            scope.clone(),
            "Archived",
            "2026-10-17T07:00:00Z",
        ),
        archived_id,
    );
    archived.session.status = AgentSessionStatus::Archived;

    let sessions = vec![
        running,
        exited,
        pending,
        wrong_owner,
        other_scope,
        other_launcher,
        archived,
    ];
    let matched = matching_running_sessions(&sessions, &scope, &codex_key, &catalog);
    assert_eq!(matched.len(), 1);
    assert_eq!(
        matched[0].session.agent_session_id, running_id,
        "only the exact running owner may be offered"
    );

    let tab_scope = AgentSessionScope::Tab { tab_id };
    let matched = matching_running_sessions(&sessions, &tab_scope, &codex_key, &catalog);
    assert_eq!(matched.len(), 1);
    assert_eq!(
        matched[0].session.title, "Other scope",
        "scope equality must follow the existing exact contract"
    );
}

/// 入口列出全部匹配会话并按更新时间倒序展示，但排序不构成自动选择；超过三项也不静默截断。
///
/// @author kongweiguang
#[test]
fn entry_matching_lists_every_running_session_newest_first() {
    let catalog = catalog();
    let codex_key = catalog
        .options
        .iter()
        .find(|option| option.provider == AgentProvider::Codex)
        .expect("default catalog has Codex")
        .key
        .clone();
    let scope = AgentSessionScope::Global;

    let mut sessions = Vec::new();
    let mut expected = Vec::new();
    for index in 0..4 {
        let id = AgentSessionId::new();
        expected.push(id.clone());
        sessions.push(with_running_terminal(
            agent_view(
                id.clone(),
                AgentProvider::Codex,
                Some(codex_key.clone()),
                scope.clone(),
                &format!("Chat {index}"),
                &format!("2026-10-17T0{index}:00:00Z"),
            ),
            id,
        ));
    }

    let matched = matching_running_sessions(&sessions, &scope, &codex_key, &catalog);
    let actual = matched
        .iter()
        .map(|view| view.session.agent_session_id.clone())
        .collect::<Vec<_>>();
    expected.reverse();
    assert_eq!(
        actual, expected,
        "all running chats stay reachable and newest-first order is display-only"
    );
}
