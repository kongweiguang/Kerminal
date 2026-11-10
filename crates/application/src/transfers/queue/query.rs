//! 来源集合只改变查询投影，不改变队列创建来源或恢复证据。
//!
//! @author kongweiguang

use std::collections::HashSet;

use kerminal_domain::{files::TransferSummary, identity::TransferId};
use tokio_util::sync::CancellationToken;

use super::TransferService;
use crate::{
    AppError, AppResult,
    transfers::{TransferQueryScope, TransferRecoveryRecord},
};

impl TransferService {
    /// All 复用权威全局快照；集合在同一记录锁内投影，保留全局序号排序及 ID 唯一性。
    ///
    /// @author kongweiguang
    pub async fn list_for_scope(&self, scope: &TransferQueryScope) -> Vec<TransferSummary> {
        if matches!(scope, TransferQueryScope::All) {
            return self.list().await;
        }
        let records = self.inner.records.lock().await;
        let mut ordered = records
            .values()
            .filter(|record| scope.matches_origin(record.origin.as_ref()))
            .collect::<Vec<_>>();
        ordered.sort_unstable_by_key(|record| std::cmp::Reverse(record.sequence));
        ordered.into_iter().map(|record| record.summary()).collect()
    }

    /// 单项动作只授权当前活记录；All 保留全局列表的拖出标记清理，但不排序或读取孤儿恢复材料。
    ///
    /// @author kongweiguang
    pub async fn get_for_scope(
        &self,
        id: &TransferId,
        scope: &TransferQueryScope,
    ) -> AppResult<TransferSummary> {
        let records = self.inner.records.lock().await;
        if matches!(scope, TransferQueryScope::All) {
            // 保持与 list 相同的加锁顺序和清理时机，避免遗留标记阻止后续任务操作。
            self.inner
                .drag_staging_ids
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retain(|id, _| records.contains_key(id));
        }
        records
            .get(id)
            .filter(|record| scope.matches_origin(record.origin.as_ref()))
            .map(|record| record.summary())
            .ok_or_else(|| AppError::not_found("传输任务不属于当前查询范围"))
    }

    /// 只筛选既有恢复查询认可的证据并按 ID 去重，不能因聚合别名复活 live 记录或丢弃旧未绑定项。
    ///
    /// @author kongweiguang
    pub async fn list_recovery_for_scope(
        &self,
        scope: &TransferQueryScope,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<TransferRecoveryRecord>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("传输恢复范围查询已取消"));
        }
        // 只读查询可以撤销等待；取消不能被仓储晚到成功或队列锁等待掩盖。
        let mut records = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("传输恢复范围查询已取消"));
            }
            records = self.list_recovery(cancellation.clone()) => records?,
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("传输恢复范围查询已取消"));
        }
        let mut seen = HashSet::new();
        records.retain(|record| {
            scope.matches_origin(record.origin.as_ref()) && seen.insert(record.id.clone())
        });
        Ok(records)
    }
}
