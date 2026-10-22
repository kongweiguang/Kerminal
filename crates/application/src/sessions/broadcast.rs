//! 交互式终端广播的目标校验用例。
//!
//! @author kongweiguang

use kerminal_domain::{
    identity::SessionId,
    sessions::{SessionInfo, SessionLifecycle},
};

use super::{SessionFilter, SessionService};
use crate::{AppError, AppResult};

/// 广播的目标上限与终端选择器一致，避免一个输入事件扩散到无界会话集合。
pub const MAX_BROADCAST_TARGETS: usize = 64;

/// 单个广播目标在校验时刻的会话状态；缺失目标保留原 ID，绝不替换为活动终端。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BroadcastTargetAvailability {
    Ready,
    NotRunning(SessionLifecycle),
    Missing,
}

/// 校验结果按请求顺序返回，供调用方逐目标展示失效原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastTargetCheck {
    pub session_id: SessionId,
    pub availability: BroadcastTargetAvailability,
}

/// 只接受用户明确选择的唯一 SessionId，并通过共享会话服务检查其当前生命周期。
///
/// 校验只用于启用广播前的快照；后续每条输入仍由调用方使用相同 ID 写入，
/// 每目标队列需继续处理校验后发生的断线和关闭竞态。
pub async fn validate_broadcast_targets(
    sessions: &dyn SessionService,
    session_ids: &[SessionId],
) -> AppResult<Vec<BroadcastTargetCheck>> {
    validate_target_ids(session_ids)?;

    let live_sessions = sessions.list(SessionFilter::default()).await?;
    Ok(project_target_checks(session_ids, &live_sessions))
}

/// 拒绝空集合、重复身份和超出交互广播预算的请求，防止边界层掩盖目标错误。
fn validate_target_ids(session_ids: &[SessionId]) -> AppResult<()> {
    if session_ids.is_empty() || session_ids.len() > MAX_BROADCAST_TARGETS {
        return Err(AppError::invalid("广播目标数量必须为 1 到 64 个"));
    }

    let mut seen: Vec<SessionId> = Vec::with_capacity(session_ids.len());
    for session_id in session_ids {
        if seen.contains(session_id) {
            return Err(AppError::invalid("广播目标不能重复"));
        }
        seen.push(session_id.clone());
    }
    Ok(())
}

/// 将共享 registry 快照投影为稳定状态，不读取 UI 焦点或工作区顺序。
fn classify_target(
    session_id: &SessionId,
    live_sessions: &[SessionInfo],
) -> BroadcastTargetAvailability {
    match live_sessions
        .iter()
        .find(|session| &session.session_id == session_id)
    {
        Some(session) if session.lifecycle == SessionLifecycle::Running => {
            BroadcastTargetAvailability::Ready
        }
        Some(session) => BroadcastTargetAvailability::NotRunning(session.lifecycle),
        None => BroadcastTargetAvailability::Missing,
    }
}

/// 保持请求给定的身份与顺序，让 UI 能对原始每个目标回报状态。
fn project_target_checks(
    session_ids: &[SessionId],
    live_sessions: &[SessionInfo],
) -> Vec<BroadcastTargetCheck> {
    session_ids
        .iter()
        .map(|session_id| BroadcastTargetCheck {
            session_id: session_id.clone(),
            availability: classify_target(session_id, live_sessions),
        })
        .collect()
}

#[cfg(test)]
#[path = "../../tests/unit/session_broadcast.rs"]
mod tests;
