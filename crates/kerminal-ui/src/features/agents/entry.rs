//! Agent 启动入口对运行中会话与未决恢复事务的精确匹配。
//!
//! @author kongweiguang

use kerminal_application::agents::{
    AgentLauncherCatalog, AgentSessionView, session_matches_launcher,
};
use kerminal_domain::{
    agents::{AgentLauncherKey, AgentSessionScope, AgentSessionStatus},
    sessions::SessionOwner,
};

/// 入口只枚举与当前精确 launcher、provider 和归属 scope 匹配、且真正由该 Agent 持有的运行中会话。
///
/// 应用层的复用助手只返回一个“最近”候选供自动复用；入口需要完整列表让用户显式选择，
/// 因此按同一身份规则枚举，且绝不按更新时间替用户决定继续哪一个。列表不设静默上限，
/// 展示区由调用方限高滚动，保证其余会话仍可明确到达。
///
/// @author kongweiguang
pub(super) fn matching_running_sessions<'a>(
    sessions: &'a [AgentSessionView],
    scope: &AgentSessionScope,
    launcher_key: &AgentLauncherKey,
    catalog: &AgentLauncherCatalog,
) -> Vec<&'a AgentSessionView> {
    let Some(option) = catalog
        .options
        .iter()
        .find(|option| &option.key == launcher_key)
    else {
        return Vec::new();
    };
    let mut matches = sessions
        .iter()
        .filter(|view| {
            view.session.status != AgentSessionStatus::Archived
                && view.session.agent_id == option.provider
                && view.session.scope.as_ref() == Some(scope)
                && is_running_owned_session(view)
                && session_matches_launcher(&view.session, launcher_key, &|launch| {
                    catalog.matches_legacy_custom_launch(launcher_key, launch)
                })
        })
        .collect::<Vec<_>>();
    // 排序只决定内联区的展示顺序；调用方不会据此自动选择第一项。
    matches.sort_by(|left, right| right.session.updated_at.cmp(&left.session.updated_at));
    matches
}

/// 没有运行中会话时，同身份的 Pending/Unknown 恢复事务仍必须阻止“进入”自动新建；
/// 用户只能从历史核对原事务，入口不重放。身份匹配复用 application 的同一 launcher 谓词。
///
/// @author kongweiguang
pub(super) fn has_pending_recovery_session(
    sessions: &[AgentSessionView],
    scope: &AgentSessionScope,
    launcher_key: &AgentLauncherKey,
    catalog: &AgentLauncherCatalog,
) -> bool {
    sessions.iter().any(|view| {
        view.session.status != AgentSessionStatus::Archived
            && view.session.scope.as_ref() == Some(scope)
            && view.resume_recovery.is_some()
            && session_matches_launcher(&view.session, launcher_key, &|launch| {
                catalog.matches_legacy_custom_launch(launcher_key, launch)
            })
    })
}

/// 只有该 Agent 自己持有 Running PTY 才算可继续的运行中会话；Pending/Unknown 恢复事务
/// 与已退出记录都不进入入口选择，仍走各自历史卡片或核对流程。owner 校验是对面板投影的
/// 防御性复核，真实继续用例仍会重新校验原会话身份。
///
/// @author kongweiguang
fn is_running_owned_session(view: &AgentSessionView) -> bool {
    super::render::has_running_agent_terminal(view)
        && view.terminal.as_ref().is_some_and(|terminal| {
            matches!(
                &terminal.owner,
                SessionOwner::Agent { agent_session_id }
                    if agent_session_id == &view.session.agent_session_id
            )
        })
}
