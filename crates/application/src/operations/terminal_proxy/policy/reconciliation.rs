//! Unknown-result 回读使用与恢复相同的互斥状态，避免同时注入或撤销脚本。
//!
//! @author kongweiguang

use kerminal_domain::identity::SessionId;

use super::{
    ApplicationStatus, FutureProxyPolicyData, FutureProxyPolicyStore, ProxyApplicationKey,
    ProxyApplicationResolution, StoredProxyApplication, lock_recover,
};

/// 回读申请区分无记录和其他 shell 操作持有互斥锁的情况。
pub(in crate::operations::terminal_proxy) enum ReconciliationReservation {
    Acquired(StoredProxyApplication),
    Busy,
    Missing,
}

impl FutureProxyPolicyStore {
    /// 对成功态或待核实态取得回读租约；apply、restore 和另一回读期间一律拒绝并发。
    pub(in crate::operations::terminal_proxy) fn begin_reconciliation(
        &self,
        key: &ProxyApplicationKey,
    ) -> ReconciliationReservation {
        let mut data = lock_recover(&self.inner);
        let Some(application) = data.applications.get_mut(key) else {
            return ReconciliationReservation::Missing;
        };
        if matches!(
            application.status,
            ApplicationStatus::Applying
                | ApplicationStatus::Reconciling
                | ApplicationStatus::Restoring
        ) {
            return ReconciliationReservation::Busy;
        }
        application.status = ApplicationStatus::Reconciling;
        ReconciliationReservation::Acquired(application.value.clone())
    }

    /// 只有已回读为活动代理才能开放撤销；未知状态继续锁住恢复入口。
    pub(in crate::operations::terminal_proxy) fn finish_reconciliation(
        &self,
        key: &ProxyApplicationKey,
        resolution: ProxyApplicationResolution,
    ) {
        self.resolve_application(key, resolution);
    }

    /// 会话确实退出或关闭后清理它独占的 shell 凭据，避免无主恢复令牌残留。
    pub(in crate::operations::terminal_proxy) fn discard_session_applications(
        &self,
        session_id: &SessionId,
    ) {
        let mut data = lock_recover(&self.inner);
        remove_session_applications(&mut data, session_id, |_| true);
    }

    /// 只有运行时已证明代次递增时清理旧 transport 凭据，保留其他代次的独立状态。
    pub(in crate::operations::terminal_proxy) fn discard_older_session_generations(
        &self,
        session_id: &SessionId,
        current_generation: u64,
    ) {
        let mut data = lock_recover(&self.inner);
        remove_session_applications(&mut data, session_id, |generation| {
            generation < current_generation
        });
    }
}

/// 按一个会话的精确代次条件删记录，并同步压缩 FIFO 索引。
fn remove_session_applications(
    data: &mut FutureProxyPolicyData,
    session_id: &SessionId,
    should_remove_generation: impl Fn(u64) -> bool,
) {
    let obsolete = data
        .applications
        .keys()
        .filter(|key| {
            key.session_id == *session_id && should_remove_generation(key.session_generation)
        })
        .cloned()
        .collect::<Vec<_>>();
    for key in obsolete {
        data.applications.remove(&key);
        data.application_order.retain(|existing| existing != &key);
    }
}
