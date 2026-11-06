//! 文件工作区的传输策略、刷新与历史清理状态。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use gpui_kit::Context;
use kerminal_application::AppResult;
use kerminal_application::transfers::TransferOrigin;
use kerminal_domain::{
    files::{ConflictPolicy, TransferFailureKind, TransferStatus, TransferSummary},
    identity::TransferId,
};
use tokio_util::sync::CancellationToken;

use super::FilesPane;
use crate::locale;

/// 清理入口同时承接终态资源重试；未知提交和可信暂停继续保留，不能因简化卡片而丢失证据。
///
/// @author kongweiguang
pub(super) fn is_clearable_history(summary: &TransferSummary) -> bool {
    matches!(
        summary.status,
        TransferStatus::Succeeded
            | TransferStatus::Skipped
            | TransferStatus::Unchanged
            | TransferStatus::Failed
            | TransferStatus::Cancelled
    ) && summary.failure_kind != Some(TransferFailureKind::CommitUnknown)
        && !(summary.status == TransferStatus::Cancelled && summary.resumable)
}

/// 只持有界面实际展示的队列状态；恢复证据与资源所有权仍由应用服务维护。
pub(super) struct TransferQueueUi {
    pub(super) conflict_policy: ConflictPolicy,
    pub(super) history_expanded: bool,
    clear_completed_cancellation: Option<CancellationToken>,
    clear_completed_origin: Option<TransferOrigin>,
    pending_retries: BTreeSet<TransferId>,
    pub(super) recovery_list_loading: bool,
    pub(super) recovery_list_error: Option<String>,
    refresh_generation: u64,
    uncertain_refresh_pending: bool,
}

impl Default for TransferQueueUi {
    /// 默认展开当前队列以便观察进度；Rename 避免未经明确选择覆盖目标。
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            conflict_policy: ConflictPolicy::Rename,
            history_expanded: true,
            clear_completed_cancellation: None,
            clear_completed_origin: None,
            pending_retries: BTreeSet::new(),
            recovery_list_loading: false,
            recovery_list_error: None,
            refresh_generation: 0,
            uncertain_refresh_pending: false,
        }
    }
}

impl TransferQueueUi {
    /// 删除事实跨快照代次保留，由最终接受的读取统一回读目录，避免迟到结果丢失补偿。
    ///
    /// @author kongweiguang
    pub(super) fn mark_uncertain_transfer_change(&mut self) {
        self.uncertain_refresh_pending = true;
    }

    /// 消费一次合并后的删除事实；失败路径也可取走并执行保守回读。
    ///
    /// @author kongweiguang
    pub(super) fn take_uncertain_transfer_change(&mut self) -> bool {
        std::mem::take(&mut self.uncertain_refresh_pending)
    }

    /// 手动刷新和队列事件共用代次，新读取使旧活任务与恢复材料回执同时失效。
    /// @author kongweiguang
    pub(super) fn begin_transfer_refresh(&mut self) -> u64 {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        self.recovery_list_loading = true;
        self.refresh_generation
    }

    /// 一次读取的活队列、恢复材料及错误提示共用同一道门禁，不能分开接受旧快照。
    /// @author kongweiguang
    pub(super) fn accepts_transfer_refresh(&self, generation: u64) -> bool {
        self.refresh_generation == generation
    }

    /// 恢复列表错误独立于文件目录状态；只接受当前读取回执，成功后才清除旧错误。
    /// @author kongweiguang
    pub(super) fn finish_recovery_refresh(
        &mut self,
        generation: u64,
        result: AppResult<()>,
    ) -> bool {
        if !self.accepts_transfer_refresh(generation) {
            return false;
        }
        self.recovery_list_loading = false;
        match result {
            Ok(()) => self.recovery_list_error = None,
            Err(error) => self.recovery_list_error = Some(super::localized_error(&error)),
        }
        true
    }

    /// 续传与历史清理互斥；资源清理与再次入队共用忙态，真实回执前不解除门禁。
    ///
    /// @author kongweiguang
    pub(super) fn begin_transfer_retry(&mut self, id: TransferId) -> bool {
        self.clear_completed_cancellation.is_none() && self.pending_retries.insert(id)
    }

    /// 即使用户已切换来源也必须释放原任务门禁，避免返回原页面后按钮永久禁用。
    ///
    /// @author kongweiguang
    pub(super) fn finish_transfer_retry(&mut self, id: &TransferId) {
        self.pending_retries.remove(id);
    }

    /// 请求启动与真实队列进度分开管理，等待锁或原 worker 收尾时也能立即反馈。
    ///
    /// @author kongweiguang
    pub(super) fn is_retrying_transfer(&self, id: &TransferId) -> bool {
        self.pending_retries.contains(id)
    }

    /// 清理会移除原任务，因此任一续传等待回执时都禁用清理入口。
    ///
    /// @author kongweiguang
    pub(super) fn has_pending_retry(&self) -> bool {
        !self.pending_retries.is_empty()
    }

    /// 清理跨来源固定目标；按钮资格也遵守面板级互斥，避免切页后发起竞争操作。
    ///
    /// @author kongweiguang
    pub(super) fn is_clearing_history(&self) -> bool {
        self.clear_completed_cancellation.is_some()
    }

    /// 同一面板只允许一个清理且不与续传并行，避免原任务先被移除后才尝试创建后继。
    ///
    /// @author kongweiguang
    pub(super) fn begin_clear_completed_history(&mut self) -> Option<CancellationToken> {
        if self.is_clearing_history() || self.has_pending_retry() {
            return None;
        }
        let cancellation = CancellationToken::new();
        self.clear_completed_cancellation = Some(cancellation.clone());
        Some(cancellation)
    }

    /// 保持 busy 状态直到服务端确认结束，取消只通知用例，不提前释放重入门闩。
    ///
    /// @author kongweiguang
    pub(super) fn finish_clear_completed_history(&mut self) -> bool {
        self.clear_completed_origin = None;
        self.clear_completed_cancellation.take().is_some()
    }

    /// 用户取消只触发当前用例 token；应用层在删除线性化点前停止并保留记录。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_clear_completed_history(&self) -> bool {
        let Some(cancellation) = self.clear_completed_cancellation.as_ref() else {
            return false;
        };
        cancellation.cancel();
        true
    }

    /// 其他来源的清理不得在当前面板显示为可取消的动作。
    ///
    /// @author kongweiguang
    pub(super) fn is_clearing_for_origin(&self, origin: Option<&TransferOrigin>) -> bool {
        self.clear_completed_cancellation.is_some()
            && self.clear_completed_origin.as_ref() == origin
    }
}

impl Drop for TransferQueueUi {
    /// 面板关闭时取消尚未越过删除线性化点的历史清理请求。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if let Some(cancellation) = self.clear_completed_cancellation.as_ref() {
            cancellation.cancel();
        }
    }
}

impl FilesPane {
    /// 后续传输按钮读取该值并写进 TransferRequest，入队后不会被 UI 更改影响。
    ///
    /// @author kongweiguang
    pub(super) fn set_conflict_policy(&mut self, policy: ConflictPolicy, cx: &mut Context<Self>) {
        self.queue.conflict_policy = policy;
        cx.notify();
    }

    /// 仅切换当前任务列表的展开状态，不为已经移除的维护界面读取冷档。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_transfer_history(&mut self, cx: &mut Context<Self>) {
        self.queue.history_expanded = !self.queue.history_expanded;
        cx.notify();
    }

    /// 阻止来源页关闭后遗失待办入口；展开后回读服务快照，使定位展示不依赖面板缓存。
    ///
    /// @author kongweiguang
    pub(crate) fn reveal_transfer_queue(&mut self, cx: &mut Context<Self>) {
        self.queue.history_expanded = true;
        self.reload_transfers(cx);
        cx.notify();
    }

    /// 清理只针对点击时的来源，先释放终态资源并归档恢复副本；失败保留原 owner 供同一入口重试。
    /// 所有资源关闭共用绝对期限，未知提交、可信暂停和系统仍持有的暂存不参与清理。
    /// 成功由队列回读直接表达，不额外留下结果提示；取消和失败仍保留可查看的反馈。
    ///
    /// @author kongweiguang
    pub(super) fn clear_completed_transfer_history(&mut self, cx: &mut Context<Self>) {
        let Some(origin) = self.transfer_origin.clone() else {
            return;
        };
        let result_origin = origin.clone();
        let Some(cancellation) = self.queue.begin_clear_completed_history() else {
            self.status = Some(
                locale::text(
                    "传输历史正在清理。",
                    "Transfer history is already being cleared.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        self.queue.clear_completed_origin = Some(origin.clone());
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |task_cancellation| async move {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    let cleanup = async {
                        for summary in transfers.list_for_origin(&origin).await {
                            if summary.cleanup_pending
                                && is_clearable_history(&summary)
                                && !transfers.is_drag_staging_pending_delivery(&summary.id)
                            {
                                transfers.retry_cleanup_until(&summary.id, deadline.into()).await?;
                            }
                        }
                        Ok::<(), kerminal_application::AppError>(())
                    };
                    task_cancellation.run_until_cancelled(cleanup).await
                        .ok_or_else(|| kerminal_application::AppError::cancelled("传输历史清理已取消"))??;
                    transfers.dismiss_completed_for_origin(&origin, task_cancellation).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.queue.finish_clear_completed_history();
                    if pane.transfer_origin.as_ref() != Some(&result_origin) {
                        pane.reload_transfers(cx);
                        return;
                    }
                    pane.status = match result {
                        Ok(_) => {
                            pane.status_details_expanded = false;
                            None
                        }
                        Err(error)
                            if error.public_code()
                                == kerminal_application::PublicErrorCode::Cancelled =>
                        {
                            Some(locale::text(
                                "已取消历史清理；需要核对或仍待清理的任务继续保留。",
                                "History cleanup was cancelled; tasks needing review or resource cleanup remain.",
                            )
                            .to_string())
                        }
                        Err(error) => Some(crate::features::files::localized_error(&error)),
                    };
                    pane.reload_transfers(cx);
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消当前清理尝试但保留 busy 状态，直到队列返回权威完成或取消结果。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_completed_transfer_history(&mut self, cx: &mut Context<Self>) {
        if self
            .queue
            .is_clearing_for_origin(self.transfer_origin.as_ref())
            && self.queue.cancel_clear_completed_history()
        {
            self.status = Some(
                locale::text(
                    "已请求取消历史清理；正在确认任务状态。",
                    "History cleanup cancellation requested; confirming queue state.",
                )
                .to_string(),
            );
            cx.notify();
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/files_transfer_queue_state.rs"]
mod tests;
