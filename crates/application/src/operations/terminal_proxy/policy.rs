//! 管理显式开启的后续终端策略与有界会话尝试状态。
//!
//! @author kongweiguang

mod reconciliation;
pub(super) use self::reconciliation::ReconciliationReservation;

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

use kerminal_domain::identity::{HostId, SessionId};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{AppError, AppResult};

use super::{FutureProxyApplyRequest, FutureProxyApplyState, TerminalProxyUnavailableReason};

const MAX_FUTURE_ATTEMPTS: usize = 512;
const MAX_TRACKED_APPLICATIONS: usize = 256;

/// 对同一会话代次和政策版本最多允许一次自动注入，避免重复 ready 通知造成重发。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct FutureAttemptKey {
    pub session_id: SessionId,
    pub session_generation: u64,
    pub policy_revision: u64,
    pub forward_id: String,
}

/// 运行时恢复脚本所需信息保存在应用内，不写进配置、shell 命令参数或历史字段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StoredProxyApplication {
    pub token: String,
    pub proxy_url: String,
    pub dialect: kerminal_domain::connections::RemoteShellDialect,
}

/// 同一终端代次下的代理应用只能有一个，防止多份备份互相覆盖恢复顺序。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct ProxyApplicationKey {
    pub session_id: SessionId,
    pub session_generation: u64,
    pub forward_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationStatus {
    Applying,
    ReadyToRestore,
    NeedsReconciliation,
    Reconciling,
    Restoring,
}

/// 已完成的 shell 操作只按已证明状态释放、保留或开放恢复，不能把未知回执当作完成。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProxyApplicationResolution {
    ReadyToRestore,
    NeedsReconciliation,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FutureAttemptStatus {
    InFlight,
    RetryableNotApplied(TerminalProxyUnavailableReason),
    Finished,
}

#[derive(Clone)]
struct FutureAttempt {
    host_id: HostId,
    forward_id: String,
    cancellation: CancellationToken,
    status: FutureAttemptStatus,
    retry_capability: Option<Uuid>,
}

#[derive(Clone)]
struct TrackedApplication {
    value: StoredProxyApplication,
    status: ApplicationStatus,
}

#[derive(Default)]
struct FutureProxyPolicyData {
    policies: HashMap<HostId, FutureProxyApplyState>,
    attempts: HashMap<FutureAttemptKey, FutureAttempt>,
    attempt_order: VecDeque<FutureAttemptKey>,
    applications: HashMap<ProxyApplicationKey, TrackedApplication>,
    application_order: VecDeque<ProxyApplicationKey>,
}

/// UI 显式策略和运行态注入凭据共享的有界状态；clone 只共享同一份数据。
#[derive(Clone, Default)]
pub struct FutureProxyPolicyStore {
    inner: Arc<Mutex<FutureProxyPolicyData>>,
}

/// 自动应用尝试的领取结果，区分 stale policy、重复事件和容量保护。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FutureAttemptClaim {
    PolicyChanged,
    AlreadyProcessed,
    TrackingCapacityReached,
}

/// 显式重试只接受由当前 attempt 发出的、尚未消费的一次性能力。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FutureAttemptRetryClaim {
    PolicyChanged,
    AttemptMissing,
    InFlight,
    NotRetryable,
    CapabilityChanged,
}

/// 应用记录的互斥结果，未成功预留前不会向终端发送脚本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApplicationReservation {
    Reserved,
    AlreadyApplied,
    TrackingCapacityReached,
}

impl FutureProxyPolicyStore {
    /// 创建一份进程内策略表；不写配置，策略生命周期由明确 UI 操作和 forward 状态管理。
    pub fn new() -> Self {
        Self::default()
    }

    /// 关闭对应转发时同步清除策略、取消运行中的尝试，并使旧重试能力因 revision 变化失效。
    pub fn cancel_by_forward(&self, forward_id: &str) -> usize {
        let mut data = lock_recover(&self.inner);
        let mut affected = 0;
        for state in data.policies.values_mut() {
            if state.forward_id.as_deref() == Some(forward_id) {
                state.forward_id = None;
                state.revision = state.revision.saturating_add(1);
                affected += 1;
            }
        }
        for attempt in data.attempts.values_mut() {
            if attempt.forward_id == forward_id {
                if matches!(attempt.status, FutureAttemptStatus::InFlight) {
                    attempt.cancellation.cancel();
                    affected += 1;
                }
                attempt.status = FutureAttemptStatus::Finished;
                attempt.retry_capability = None;
            }
        }
        affected
    }

    /// 按完整会话代次寻找唯一可核对的原转发，只返回 ID，不把内存恢复凭据越过应用层。
    ///
    /// @author kongweiguang
    pub(super) fn unique_application_forward_id(
        &self,
        session_id: &SessionId,
        session_generation: u64,
    ) -> AppResult<Option<String>> {
        let data = lock_recover(&self.inner);
        let mut matching = data.applications.keys().filter(|key| {
            key.session_id == *session_id && key.session_generation == session_generation
        });
        let Some(key) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            return Err(AppError::conflict(
                "当前终端存在多个代理恢复记录，无法安全选择原转发",
            ));
        }
        Ok(Some(key.forward_id.clone()))
    }

    /// 应用退出时清空自动策略并取消运行中的尝试；旧重试能力随 revision 变化失效。
    pub fn cancel_all(&self) -> usize {
        let mut data = lock_recover(&self.inner);
        let mut affected = 0;
        for state in data.policies.values_mut() {
            if state.forward_id.take().is_some() {
                state.revision = state.revision.saturating_add(1);
                affected += 1;
            }
        }
        for attempt in data.attempts.values_mut() {
            if matches!(attempt.status, FutureAttemptStatus::InFlight) {
                attempt.cancellation.cancel();
                affected += 1;
            }
            attempt.status = FutureAttemptStatus::Finished;
            attempt.retry_capability = None;
        }
        affected
    }

    /// 返回主机独立策略快照；未设置时 revision 从零开始且 forward_id 为空。
    pub(super) fn state(&self, host_id: &HostId) -> FutureProxyApplyState {
        lock_recover(&self.inner)
            .policies
            .get(host_id)
            .cloned()
            .unwrap_or_else(|| FutureProxyApplyState {
                host_id: host_id.clone(),
                revision: 0,
                forward_id: None,
            })
    }

    /// 以 revision CAS 写入显式策略，并取消旧策略下运行中的尝试；完成态 intent 由 revision 拒绝。
    pub(super) fn set(&self, request: FutureProxyApplyRequest) -> AppResult<FutureProxyApplyState> {
        let mut data = lock_recover(&self.inner);
        let current = data
            .policies
            .get(&request.host_id)
            .cloned()
            .unwrap_or_else(|| FutureProxyApplyState {
                host_id: request.host_id.clone(),
                revision: 0,
                forward_id: None,
            });
        if current.revision != request.expected_revision {
            return Err(AppError::conflict("后续代理策略已被其他操作更新"));
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| AppError::infrastructure("后续代理策略 revision 已耗尽"))?;
        for attempt in data.attempts.values_mut() {
            if attempt.host_id == request.host_id {
                if matches!(attempt.status, FutureAttemptStatus::InFlight) {
                    attempt.cancellation.cancel();
                }
                attempt.status = FutureAttemptStatus::Finished;
                attempt.retry_capability = None;
            }
        }
        let next = FutureProxyApplyState {
            host_id: request.host_id.clone(),
            revision,
            forward_id: request.forward_id,
        };
        data.policies.insert(request.host_id, next.clone());
        Ok(next)
    }

    /// 在 policy revision 仍匹配时领取唯一自动尝试，并返回 forward 停止时可取消的 token。
    ///
    /// @author kongweiguang
    pub(super) fn claim_attempt(
        &self,
        host_id: &HostId,
        forward_id: &str,
        key: FutureAttemptKey,
    ) -> Result<CancellationToken, FutureAttemptClaim> {
        let mut data = lock_recover(&self.inner);
        let matches_policy = data.policies.get(host_id).is_some_and(|state| {
            state.revision == key.policy_revision
                && state.forward_id.as_deref() == Some(forward_id)
                && key.forward_id == forward_id
        });
        if !matches_policy {
            return Err(FutureAttemptClaim::PolicyChanged);
        }
        if data.attempts.contains_key(&key) {
            return Err(FutureAttemptClaim::AlreadyProcessed);
        }
        if !reserve_attempt_slot(&mut data) {
            return Err(FutureAttemptClaim::TrackingCapacityReached);
        }
        let cancellation = CancellationToken::new();
        data.attempt_order.push_back(key.clone());
        data.attempts.insert(
            key,
            FutureAttempt {
                host_id: host_id.clone(),
                forward_id: forward_id.to_owned(),
                cancellation: cancellation.clone(),
                status: FutureAttemptStatus::InFlight,
                retry_capability: None,
            },
        );
        Ok(cancellation)
    }

    /// 先落定完成态，再仅为当前策略有效且未取消的提示符瞬态结果签发重试能力，避免重放未知副作用。
    ///
    /// @author kongweiguang
    pub(super) fn finish_attempt(
        &self,
        host_id: &HostId,
        key: &FutureAttemptKey,
        retry_reason: Option<TerminalProxyUnavailableReason>,
        operation_cancelled: bool,
    ) -> Option<Uuid> {
        let mut data = lock_recover(&self.inner);
        let policy_matches = data.policies.get(host_id).is_some_and(|state| {
            state.revision == key.policy_revision
                && state.forward_id.as_deref() == Some(key.forward_id.as_str())
        });
        let attempt = data.attempts.get_mut(key)?;
        if attempt.host_id != *host_id
            || attempt.forward_id != key.forward_id
            || !matches!(attempt.status, FutureAttemptStatus::InFlight)
        {
            return None;
        }

        attempt.status = FutureAttemptStatus::Finished;
        attempt.retry_capability = None;
        if operation_cancelled || attempt.cancellation.is_cancelled() || !policy_matches {
            return None;
        }
        let reason = retry_reason.filter(|reason| reason.is_safe_future_retry())?;
        let capability = Uuid::new_v4();
        attempt.status = FutureAttemptStatus::RetryableNotApplied(reason);
        attempt.retry_capability = Some(capability);
        Some(capability)
    }

    /// 原子消费尚未失效的重试能力，并使唯一 attempt 回到可等待状态。
    ///
    /// @author kongweiguang
    pub(super) fn begin_retry(
        &self,
        host_id: &HostId,
        key: &FutureAttemptKey,
        capability: Uuid,
    ) -> Result<CancellationToken, FutureAttemptRetryClaim> {
        let mut data = lock_recover(&self.inner);
        let policy_matches = data.policies.get(host_id).is_some_and(|state| {
            state.revision == key.policy_revision
                && state.forward_id.as_deref() == Some(key.forward_id.as_str())
        });
        if !policy_matches {
            return Err(FutureAttemptRetryClaim::PolicyChanged);
        }
        let attempt = data
            .attempts
            .get_mut(key)
            .ok_or(FutureAttemptRetryClaim::AttemptMissing)?;
        if attempt.host_id != *host_id || attempt.forward_id != key.forward_id {
            return Err(FutureAttemptRetryClaim::AttemptMissing);
        }
        match attempt.status {
            FutureAttemptStatus::InFlight => return Err(FutureAttemptRetryClaim::InFlight),
            FutureAttemptStatus::RetryableNotApplied(reason) if reason.is_safe_future_retry() => {}
            FutureAttemptStatus::RetryableNotApplied(_) | FutureAttemptStatus::Finished => {
                return Err(FutureAttemptRetryClaim::NotRetryable);
            }
        }
        if attempt.retry_capability != Some(capability) {
            return Err(FutureAttemptRetryClaim::CapabilityChanged);
        }
        let cancellation = CancellationToken::new();
        attempt.cancellation = cancellation.clone();
        attempt.status = FutureAttemptStatus::InFlight;
        attempt.retry_capability = None;
        Ok(cancellation)
    }

    /// 等待提示符期间同时核验策略版本和尝试令牌，避免转发停止后继续注入。
    pub(super) fn policy_is_current(
        &self,
        host_id: &HostId,
        revision: u64,
        key: &FutureAttemptKey,
    ) -> bool {
        let data = lock_recover(&self.inner);
        let policy_matches = data.policies.get(host_id).is_some_and(|state| {
            state.revision == revision
                && state.forward_id.as_deref() == Some(key.forward_id.as_str())
        });
        let attempt_is_active = data.attempts.get(key).is_some_and(|attempt| {
            attempt.host_id == *host_id
                && attempt.forward_id == key.forward_id
                && matches!(attempt.status, FutureAttemptStatus::InFlight)
                && !attempt.cancellation.is_cancelled()
        });
        policy_matches && attempt_is_active
    }

    /// 预留可恢复状态后才能注入，满载时 fail closed 而不丢弃其他活动会话的恢复凭据。
    pub(super) fn reserve_application(
        &self,
        key: ProxyApplicationKey,
        value: StoredProxyApplication,
    ) -> ApplicationReservation {
        let mut data = lock_recover(&self.inner);
        let same_session = data.applications.keys().any(|existing| {
            existing.session_id == key.session_id
                && existing.session_generation == key.session_generation
        });
        if same_session {
            return ApplicationReservation::AlreadyApplied;
        }
        remove_other_generations(&mut data, &key.session_id, key.session_generation);
        if data.applications.len() >= MAX_TRACKED_APPLICATIONS {
            return ApplicationReservation::TrackingCapacityReached;
        }
        data.application_order.push_back(key.clone());
        data.applications.insert(
            key,
            TrackedApplication {
                value,
                status: ApplicationStatus::Applying,
            },
        );
        ApplicationReservation::Reserved
    }

    /// 取出恢复凭据并设置互斥标记，重复点击不会并发执行两份撤销脚本。
    pub(super) fn begin_restore(
        &self,
        key: &ProxyApplicationKey,
    ) -> Option<StoredProxyApplication> {
        let mut data = lock_recover(&self.inner);
        let application = data.applications.get_mut(key)?;
        if application.status != ApplicationStatus::ReadyToRestore {
            return None;
        }
        application.status = ApplicationStatus::Restoring;
        Some(application.value.clone())
    }

    /// 按本次写入或回读能证明的结果原子更新状态，不留下可并发点击的中间状态。
    pub(super) fn resolve_application(
        &self,
        key: &ProxyApplicationKey,
        resolution: ProxyApplicationResolution,
    ) {
        let mut data = lock_recover(&self.inner);
        match resolution {
            ProxyApplicationResolution::Remove => {
                data.applications.remove(key);
                data.application_order.retain(|existing| existing != key);
            }
            ProxyApplicationResolution::ReadyToRestore => {
                if let Some(application) = data.applications.get_mut(key) {
                    application.status = ApplicationStatus::ReadyToRestore;
                }
            }
            ProxyApplicationResolution::NeedsReconciliation => {
                if let Some(application) = data.applications.get_mut(key) {
                    application.status = ApplicationStatus::NeedsReconciliation;
                }
            }
        }
    }
}

/// 清理旧 transport 代次的脚本凭据；重连 shell 不再共享旧进程环境。
fn remove_other_generations(
    data: &mut FutureProxyPolicyData,
    session_id: &SessionId,
    current_generation: u64,
) {
    let obsolete: Vec<_> = data
        .applications
        .keys()
        .filter(|key| key.session_id == *session_id && key.session_generation != current_generation)
        .cloned()
        .collect();
    for key in obsolete {
        data.applications.remove(&key);
        data.application_order.retain(|existing| existing != &key);
    }
}

/// 只淘汰已完成的旧尝试；如果所有槽位都在执行，则拒绝新增以维持明确去重边界。
fn reserve_attempt_slot(data: &mut FutureProxyPolicyData) -> bool {
    while data.attempts.len() >= MAX_FUTURE_ATTEMPTS {
        let completed_index = data.attempt_order.iter().position(|key| {
            data.attempts
                .get(key)
                .is_none_or(|attempt| !matches!(attempt.status, FutureAttemptStatus::InFlight))
        });
        let Some(index) = completed_index else {
            return false;
        };
        if let Some(key) = data.attempt_order.remove(index) {
            data.attempts.remove(&key);
        }
    }
    true
}

/// 锁意外中毒时保留已提交的策略/恢复凭据，避免让资源清理路径崩溃。
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
