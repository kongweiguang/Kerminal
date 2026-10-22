//! Agent 聚合和目标绑定的公开领域行为回归。
//!
//! @author kongweiguang

use kerminal_domain::agents::{
    AgentDomainError, AgentLauncherKey, AgentProvider, AgentSession, AgentSessionLaunch,
    AgentSessionScope, AgentSessionStatus, AgentTargetBinding, AgentTargetLiveStatus,
};
use kerminal_domain::identity::{AgentSessionId, PaneId, SessionId, TabId};

/// 组装合法的 Codex 聚合，令每个回归聚焦在单一不变量上。
fn valid_session(target: Option<AgentTargetBinding>) -> AgentSession {
    AgentSession::new(
        AgentSessionId::new(),
        AgentProvider::Codex,
        Some(AgentLauncherKey::builtin(AgentProvider::Codex).expect("builtin key is valid")),
        "Codex work".to_owned(),
        "2026-10-16T00:00:00Z".to_owned(),
        "C:/test/.kerminal".to_owned(),
        "C:/test/.kerminal/agents/sessions/test".to_owned(),
        AgentSessionLaunch {
            command_label: "codex".to_owned(),
            shell: "codex".to_owned(),
            args: vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
            cwd: "C:/test/.kerminal/agents/sessions/test".to_owned(),
        },
        Some(AgentSessionScope::Global),
        target,
    )
    .expect("test session is valid")
}

/// 目标 generation 过期或被不同 session 重用时拒绝，防止异步 UI 事件覆盖新绑定。
#[test]
fn binding_generation_rejects_stale_and_reused_values() {
    let first = AgentTargetBinding {
        binding_id: Some("binding-a".to_owned()),
        generation: 5,
        pane_id: Some(PaneId::new()),
        tab_id: Some(TabId::new()),
        terminal_session_id: Some(SessionId::new()),
        target_ref: Some("local".to_owned()),
        target_kind: Some("local".to_owned()),
        cwd: Some("C:/workspace".to_owned()),
        shell: Some("pwsh".to_owned()),
        live_status: AgentTargetLiveStatus::Ready,
        last_seen_at: None,
    };
    let mut session = valid_session(Some(first.clone()));
    let mut stale = first.clone();
    stale.generation = 4;
    assert_eq!(
        session.bind_preferred_target(stale, "2026-10-16T00:01:00Z".to_owned()),
        Err(AgentDomainError::StaleBindingGeneration {
            current: 5,
            requested: 4
        })
    );

    let mut reused = first.clone();
    reused.terminal_session_id = Some(SessionId::new());
    assert_eq!(
        session.bind_preferred_target(reused, "2026-10-16T00:02:00Z".to_owned()),
        Err(AgentDomainError::BindingGenerationReused(5))
    );
}

/// 首选目标只解析显式 SessionId；Ready 缺 ID 和已关闭目标都不能回退到其它终端。
#[test]
fn preferred_target_never_falls_back_when_missing_or_closed() {
    let mut missing_id = AgentTargetBinding::unbound(1);
    missing_id.live_status = AgentTargetLiveStatus::Ready;
    assert_eq!(
        missing_id.preferred_session_id(),
        Err(AgentDomainError::PreferredTargetUnavailable)
    );
    assert_eq!(
        AgentTargetBinding {
            live_status: AgentTargetLiveStatus::Closed,
            ..AgentTargetBinding::unbound(2)
        }
        .preferred_session_id(),
        Err(AgentDomainError::PreferredTargetUnavailable)
    );
}

/// 首选 target 的失效不终止 Agent CLI 会话，目标不在有效时绝不隐式换绑。
#[test]
fn preferred_target_staleness_is_independent_from_agent_cli_lifecycle() {
    let target_session_id = SessionId::new();
    let binding = AgentTargetBinding {
        terminal_session_id: Some(target_session_id.clone()),
        live_status: AgentTargetLiveStatus::Ready,
        ..AgentTargetBinding::unbound(1)
    };
    let mut session = valid_session(Some(binding.clone()));
    session
        .mark_target_stale(AgentTargetLiveStatus::Closed, "t1".to_owned())
        .expect("explicit target invalidation is valid");
    assert_eq!(session.status, AgentSessionStatus::Active);
    assert_eq!(
        session.target.as_ref().map(|target| target.live_status),
        Some(AgentTargetLiveStatus::Closed)
    );

    let mut rebound = binding;
    rebound.generation = 2;
    rebound.terminal_session_id = Some(SessionId::new());
    session
        .bind_preferred_target(rebound, "t2".to_owned())
        .expect("explicit rebind recovers only preferred-target state");
    assert_eq!(session.status, AgentSessionStatus::Active);
}

/// CLI 退出后的显式目标重绑只修复 binding，不得伪造 Agent 进程已恢复。
#[test]
fn rebinding_does_not_reactivate_an_exited_agent_cli() {
    let binding = AgentTargetBinding {
        terminal_session_id: Some(SessionId::new()),
        live_status: AgentTargetLiveStatus::Ready,
        ..AgentTargetBinding::unbound(1)
    };
    let mut session = valid_session(Some(binding.clone()));
    session.mark_cli_stale("cli-exited".to_owned());
    let mut rebound = binding;
    rebound.generation = 2;
    rebound.terminal_session_id = Some(SessionId::new());
    session
        .bind_preferred_target(rebound, "target-rebound".to_owned())
        .expect("target can be explicitly rebound");
    assert_eq!(session.status, AgentSessionStatus::Stale);
}

/// 失效方法仅接受 stale/closed，并在无绑定时失败且不改变 Agent CLI 状态。
#[test]
fn target_invalidation_requires_an_existing_target_and_invalidating_status() {
    let mut session = valid_session(None);
    assert_eq!(
        session.mark_target_stale(AgentTargetLiveStatus::Ready, "t1".to_owned()),
        Err(AgentDomainError::InvalidTargetInvalidationStatus)
    );
    assert_eq!(
        session.mark_target_stale(AgentTargetLiveStatus::Stale, "t2".to_owned()),
        Err(AgentDomainError::TargetBindingMissing)
    );
    assert_eq!(session.status, AgentSessionStatus::Active);
}

/// 历史恢复入口拒绝未知 schema 和不能启动的快照，防止文件数据绕过领域校验。
#[test]
fn restore_rejects_future_schema_and_invalid_ready_binding() {
    let mut target = AgentTargetBinding::unbound(0);
    target.live_status = AgentTargetLiveStatus::Ready;
    let session = valid_session(None);
    assert_eq!(
        AgentSession::restore(
            2,
            session.agent_session_id.clone(),
            session.agent_id,
            session.launcher_key.clone(),
            session.title.clone(),
            session.created_at.clone(),
            session.updated_at.clone(),
            session.status,
            session.workspace_root.clone(),
            session.session_root.clone(),
            session.launch.clone(),
            session.scope.clone(),
            None,
        )
        .unwrap_err(),
        AgentDomainError::UnsupportedSchemaVersion(2)
    );
    assert_eq!(
        AgentSession::restore(
            1,
            session.agent_session_id.clone(),
            session.agent_id,
            session.launcher_key.clone(),
            session.title.clone(),
            session.created_at.clone(),
            session.updated_at.clone(),
            session.status,
            session.workspace_root.clone(),
            session.session_root.clone(),
            session.launch.clone(),
            session.scope.clone(),
            Some(target),
        )
        .unwrap_err(),
        AgentDomainError::PreferredTargetUnavailable
    );
}

/// Debug 输出不展开启动参数或会话路径，避免诊断日志泄露 CLI 凭据。
#[test]
fn debug_redacts_launch_arguments_and_paths() {
    let mut session = valid_session(None);
    session
        .launch
        .args
        .push("--token=secret-argument".to_owned());
    let debug = format!("{session:?} {:?}", session.launch);
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("secret-argument"));
    assert!(!debug.contains("C:/test/.kerminal"));
}
