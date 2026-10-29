//! Agent 启动入口按 scope、launcher 与实际权限模式复用历史会话。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString};

use kerminal_application::agents::{
    AgentResumeCapability, AgentResumeRecovery, AgentResumeRecoveryStatus, AgentSessionView,
    find_reusable_agent_session,
};
use kerminal_domain::{
    agents::{
        AgentLaunchPermissionMode, AgentLauncherKey, AgentProvider, AgentSession,
        AgentSessionLaunch, AgentSessionScope, AgentSessionStatus,
    },
    identity::{AgentSessionId, SessionId},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};

/// 构造拥有完整稳定启动身份的测试会话，不读写真实配置或终端。
fn view(
    key: Option<AgentLauncherKey>,
    provider: AgentProvider,
    args: Vec<String>,
    scope: AgentSessionScope,
    updated_at: &str,
) -> AgentSessionView {
    let mut session = AgentSession::new(
        AgentSessionId::new(),
        provider,
        key,
        provider.as_str().to_owned(),
        "2026-10-27T00:00:00Z".to_owned(),
        "C:\\Users\\test\\.kerminal".to_owned(),
        "C:\\Users\\test\\.kerminal\\agents\\session".to_owned(),
        AgentSessionLaunch {
            command_label: provider.as_str().to_owned(),
            shell: provider.as_str().to_owned(),
            args,
            cwd: "C:\\Users\\test\\.kerminal\\agents\\session".to_owned(),
        },
        Some(scope),
        None,
    )
    .expect("test session has a valid launch snapshot");
    session.updated_at = updated_at.to_owned();
    AgentSessionView {
        session,
        terminal: None,
        resume_recovery: None,
        resume_capability: AgentResumeCapability::Supported,
    }
}

/// 构造区分实际运行进程与持久会话记录的终端投影。
///
/// @author kongweiguang
fn terminal_for(agent_session_id: &AgentSessionId, lifecycle: SessionLifecycle) -> SessionInfo {
    SessionInfo {
        session_id: SessionId::new(),
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-agent"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Agent {
            agent_session_id: agent_session_id.clone(),
        },
        size: TerminalSize::default(),
        lifecycle,
        exit_status: None,
    }
}

/// 将已持久化记录转换为 CLI 退出后核对得到的 Stale 状态。
///
/// @author kongweiguang
fn persisted(mut view: AgentSessionView) -> AgentSessionView {
    view.session.status = AgentSessionStatus::Stale;
    view.terminal = Some(terminal_for(
        &view.session.agent_session_id,
        SessionLifecycle::Exited,
    ));
    view
}

/// 为精确匹配的会话附加运行中 CLI，验证其优先于更新的持久历史。
///
/// @author kongweiguang
fn running(mut view: AgentSessionView) -> AgentSessionView {
    view.terminal = Some(terminal_for(
        &view.session.agent_session_id,
        SessionLifecycle::Running,
    ));
    view
}

/// 命中运行进程时优先于较新的历史，保证 Enter 回到活动对话。
///
/// @author kongweiguang
#[test]
fn prefers_exact_running_session_over_newer_persisted_match() {
    let key = AgentLauncherKey::builtin(AgentProvider::Codex).expect("builtin key");
    let scope = AgentSessionScope::Global;
    let older = running(view(
        Some(key.clone()),
        AgentProvider::Codex,
        vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
        scope.clone(),
        "2026-10-26T00:00:00Z",
    ));
    let latest = persisted(view(
        Some(key.clone()),
        AgentProvider::Codex,
        vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
        scope.clone(),
        "2026-10-27T00:00:00Z",
    ));
    let wrong_scope = persisted(view(
        Some(key.clone()),
        AgentProvider::Codex,
        vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
        AgentSessionScope::Tab {
            tab_id: kerminal_domain::identity::TabId::new(),
        },
        "2026-10-28T00:00:00Z",
    ));

    let sessions = [older, latest, wrong_scope];
    let found = find_reusable_agent_session(
        &sessions,
        &scope,
        &key,
        AgentLaunchPermissionMode::SkipPermissions,
        |_| false,
    )
    .expect("same-scope running match exists");
    assert_eq!(found.session.updated_at, "2026-10-26T00:00:00Z");
}

/// 精确权限历史优先于较新的标准权限历史；仅 skipPermissions 入口可安全回退。
///
/// @author kongweiguang
#[test]
fn exact_permission_history_precedes_safe_legacy_permission_fallback() {
    let key = AgentLauncherKey::builtin(AgentProvider::Codex).expect("builtin key");
    let scope = AgentSessionScope::Global;
    let standard = persisted(view(
        Some(key.clone()),
        AgentProvider::Codex,
        Vec::new(),
        scope.clone(),
        "2026-10-28T00:00:00Z",
    ));
    let elevated = persisted(view(
        Some(key.clone()),
        AgentProvider::Codex,
        vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
        scope.clone(),
        "2026-10-26T00:00:00Z",
    ));
    let sessions = [standard, elevated];

    let full = find_reusable_agent_session(
        &sessions,
        &scope,
        &key,
        AgentLaunchPermissionMode::SkipPermissions,
        |_| false,
    )
    .expect("exact skipPermissions history is restorable");
    assert_eq!(
        full.session.permission_mode,
        AgentLaunchPermissionMode::SkipPermissions
    );
    let standard = find_reusable_agent_session(
        &sessions,
        &scope,
        &key,
        AgentLaunchPermissionMode::Default,
        |_| false,
    )
    .expect("same default mode can reuse the standard session");
    assert_eq!(
        standard.session.permission_mode,
        AgentLaunchPermissionMode::Default
    );
}

/// 完整权限入口不能接管仍在运行的标准权限会话。
///
/// @author kongweiguang
#[test]
fn permission_fallback_never_reuses_a_running_standard_session() {
    let scope = AgentSessionScope::Global;
    let key = AgentLauncherKey::builtin(AgentProvider::Codex).expect("builtin key");
    let standard = running(view(
        Some(key.clone()),
        AgentProvider::Codex,
        Vec::new(),
        scope.clone(),
        "2026-10-27T00:00:00Z",
    ));
    assert!(
        find_reusable_agent_session(
            &[standard],
            &scope,
            &key,
            AgentLaunchPermissionMode::SkipPermissions,
            |_| false,
        )
        .is_none()
    );
}

/// Pending 或 Unknown 恢复事务保持选中，Enter 应先核对而不是另启 CLI。
///
/// @author kongweiguang
#[test]
fn unresolved_exact_recovery_precedes_restorable_history() {
    let key = AgentLauncherKey::builtin(AgentProvider::Codex).expect("builtin key");
    let scope = AgentSessionScope::Global;
    let mut recovery = persisted(view(
        Some(key.clone()),
        AgentProvider::Codex,
        vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
        scope.clone(),
        "2026-10-26T00:00:00Z",
    ));
    recovery.resume_recovery = Some(AgentResumeRecovery {
        terminal_session_id: SessionId::new(),
        status: AgentResumeRecoveryStatus::Unknown,
    });
    let newer = persisted(view(
        Some(key.clone()),
        AgentProvider::Codex,
        vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
        scope.clone(),
        "2026-10-27T00:00:00Z",
    ));

    let sessions = [recovery, newer];
    let selected = find_reusable_agent_session(
        &sessions,
        &scope,
        &key,
        AgentLaunchPermissionMode::SkipPermissions,
        |_| false,
    )
    .expect("unresolved recovery must be selected for reconciliation");
    assert!(selected.resume_recovery.is_some());
}

/// 旧内置会话可推导 key；旧 Custom 必须匹配当前配置命令的启动语义。
///
/// @author kongweiguang
#[test]
fn legacy_builtin_key_is_inferred_but_custom_requires_matching_command() {
    let scope = AgentSessionScope::Global;
    let legacy_builtin = persisted(view(
        None,
        AgentProvider::Pi,
        Vec::new(),
        scope.clone(),
        "2026-10-27T00:00:00Z",
    ));
    let legacy_builtin_sessions = [legacy_builtin];
    let found = find_reusable_agent_session(
        &legacy_builtin_sessions,
        &scope,
        &AgentLauncherKey::builtin(AgentProvider::Pi).expect("builtin key"),
        AgentLaunchPermissionMode::Default,
        |_| false,
    );
    assert!(found.is_some());
    assert!(AgentLaunchPermissionMode::Default.can_reuse(AgentLaunchPermissionMode::Default));

    let custom_key = AgentLauncherKey::new(
        "custom:11111111-1111-4111-8111-111111111111",
        AgentProvider::Custom,
    )
    .expect("custom key");
    let mut legacy_custom = persisted(view(
        None,
        AgentProvider::Custom,
        vec!["--mode".to_owned()],
        scope.clone(),
        "2026-10-27T00:00:00Z",
    ));
    legacy_custom.session.launch.command_label = "review-cli --mode".to_owned();
    assert!(
        find_reusable_agent_session(
            std::slice::from_ref(&legacy_custom),
            &scope,
            &custom_key,
            AgentLaunchPermissionMode::Default,
            |launch| launch.command_label.trim() == "review-cli --mode",
        )
        .is_some()
    );
    assert!(
        find_reusable_agent_session(
            &[legacy_custom],
            &scope,
            &custom_key,
            AgentLaunchPermissionMode::Default,
            |_| false,
        )
        .is_none()
    );
}
