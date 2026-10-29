//! Agent 启动入口的历史会话匹配。
//!
//! @author kongweiguang

use kerminal_domain::agents::{
    AgentLaunchPermissionMode, AgentLauncherKey, AgentProvider, AgentSessionLaunch,
    AgentSessionScope, AgentSessionStatus,
};
use kerminal_domain::sessions::SessionLifecycle;

use super::{AgentResumeCapability, AgentSessionView};

/// 依次选择 main 的精确运行会话、可恢复历史，再按安全方向回退旧权限历史。
///
/// 未核对的恢复事务排在新建之前，避免 Enter 重复启动 CLI。
///
/// @author kongweiguang
pub fn find_reusable_agent_session<'a>(
    sessions: &'a [AgentSessionView],
    scope: &AgentSessionScope,
    launcher_key: &AgentLauncherKey,
    permission_mode: AgentLaunchPermissionMode,
    legacy_custom_launch_matches: impl Fn(&AgentSessionLaunch) -> bool,
) -> Option<&'a AgentSessionView> {
    let matching = sessions
        .iter()
        .filter(|view| {
            let session = &view.session;
            session.status != AgentSessionStatus::Archived
                && session.scope.as_ref() == Some(scope)
                && session_matches_launcher(session, launcher_key, &legacy_custom_launch_matches)
        })
        .collect::<Vec<_>>();

    newest_matching(&matching, |view| {
        view.session.permission_mode == permission_mode && is_running(view)
    })
    .or_else(|| {
        newest_matching(&matching, |view| {
            view.session.permission_mode == permission_mode && view.resume_recovery.is_some()
        })
    })
    .or_else(|| {
        newest_matching(&matching, |view| {
            view.session.permission_mode == permission_mode && is_restorable(view)
        })
    })
    .or_else(|| {
        (permission_mode == AgentLaunchPermissionMode::SkipPermissions
            && matches!(launcher_key.as_str(), "builtin:codex" | "builtin:claude"))
        .then(|| {
            newest_matching(&matching, |view| {
                view.session.permission_mode == AgentLaunchPermissionMode::Default
                    && is_restorable(view)
            })
        })
        .flatten()
    })
}

/// 在同一语义优先级内返回最近更新的候选。
///
/// @author kongweiguang
fn newest_matching<'a>(
    candidates: &[&'a AgentSessionView],
    predicate: impl Fn(&AgentSessionView) -> bool,
) -> Option<&'a AgentSessionView> {
    candidates
        .iter()
        .copied()
        .filter(|view| predicate(view))
        .max_by(|left, right| left.session.updated_at.cmp(&right.session.updated_at))
}

/// 只有实际运行的 PTY 才算运行会话；退出或重启中的 Active 记录按历史处理。
///
/// @author kongweiguang
fn is_running(view: &AgentSessionView) -> bool {
    view.session.status == AgentSessionStatus::Active
        && view.resume_recovery.is_none()
        && view
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
}

/// 仅允许继续恢复能力已确认且旧终端已退出的会话。
///
/// @author kongweiguang
fn is_restorable(view: &AgentSessionView) -> bool {
    view.resume_capability == AgentResumeCapability::Supported
        && view.session.status != AgentSessionStatus::Archived
        && view.resume_recovery.is_none()
        && view.terminal.as_ref().is_none_or(|terminal| {
            matches!(
                terminal.lifecycle,
                SessionLifecycle::Exited | SessionLifecycle::Closed | SessionLifecycle::Failed
            )
        })
}

/// 显式 key 始终优先；仅旧记录缺 key 时才按启动快照做兼容匹配。
///
/// @author kongweiguang
fn session_matches_launcher(
    session: &kerminal_domain::agents::AgentSession,
    requested: &AgentLauncherKey,
    legacy_custom_launch_matches: &impl Fn(&AgentSessionLaunch) -> bool,
) -> bool {
    if let Some(stored) = session.launcher_key.as_ref() {
        return stored == requested;
    }
    match session.agent_id {
        AgentProvider::Custom => {
            requested.as_str().starts_with("custom:")
                && legacy_custom_launch_matches(&session.launch)
        }
        provider => {
            AgentLauncherKey::builtin(provider).is_ok_and(|legacy_key| &legacy_key == requested)
        }
    }
}
