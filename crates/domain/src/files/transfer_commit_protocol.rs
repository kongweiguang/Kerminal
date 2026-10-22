//! 多项目传输的逐项提交状态；回执、可取消窗口与最终结果屏障由领域规则约束。
//!
//! @author kongweiguang

use crate::files::transfer_outcome::{TransferCommitSummary, TransferCommitSummaryOverflow};

/// 单个 atomic commit 的 receipt 或最终结果尚未形成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitPhase {
    Ready,
    ItemInProgress,
    BetweenItems,
    Finalized,
    OutcomeUnknown,
}

/// 不符合提交边界的状态迁移必须拒绝，不能依靠调用顺序约定伪造结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferCommitTransitionError {
    ItemAlreadyInProgress,
    NoItemInProgress,
    CancellationRequested,
    NoConfirmedReceipt,
    NoChangesNotAllowed,
    AlreadyFinalized,
    OutcomeUnknown,
    SummaryOverflow,
}

/// 目录合并逐项保留确认计数；未知项不计数且不能安全自动重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferCommitProtocol {
    phase: CommitPhase,
    summary: TransferCommitSummary,
    cancel_requested: bool,
}

impl Default for TransferCommitProtocol {
    /// 新任务从尚未启动目标提交的阶段开始。
    fn default() -> Self {
        Self {
            phase: CommitPhase::Ready,
            summary: TransferCommitSummary::empty(),
            cancel_requested: false,
        }
    }
}

impl TransferCommitProtocol {
    /// 只有未完成的多项任务能开始下一项提交，阻止最终屏障后再次写目标。
    pub fn begin_item(&mut self) -> Result<(), TransferCommitTransitionError> {
        if self.cancel_requested {
            return Err(TransferCommitTransitionError::CancellationRequested);
        }
        match self.phase {
            CommitPhase::Ready | CommitPhase::BetweenItems => {
                self.phase = CommitPhase::ItemInProgress;
                Ok(())
            }
            CommitPhase::ItemInProgress => {
                Err(TransferCommitTransitionError::ItemAlreadyInProgress)
            }
            CommitPhase::Finalized => Err(TransferCommitTransitionError::AlreadyFinalized),
            CommitPhase::OutcomeUnknown => Err(TransferCommitTransitionError::OutcomeUnknown),
        }
    }

    /// 记录用户取消意图；当前原子项等待明确回执，项间则阻止后续目标修改。
    pub fn request_cancel(&mut self) {
        if !matches!(
            self.phase,
            CommitPhase::Finalized | CommitPhase::OutcomeUnknown
        ) {
            self.cancel_requested = true;
        }
    }

    /// 获得单项目标提交明确回执后更新摘要并开放项间取消窗口。
    pub fn record_confirmed_receipt(
        &mut self,
        committed_bytes: u64,
    ) -> Result<TransferCommitSummary, TransferCommitTransitionError> {
        if self.phase != CommitPhase::ItemInProgress {
            return Err(TransferCommitTransitionError::NoItemInProgress);
        }
        let mut next = self.summary;
        next.record_confirmed_entry(committed_bytes)
            .map_err(|TransferCommitSummaryOverflow| {
                TransferCommitTransitionError::SummaryOverflow
            })?;
        self.summary = next;
        self.phase = CommitPhase::BetweenItems;
        Ok(self.summary)
    }

    /// 服务端明确拒绝了无副作用的当前项时释放许可，不把已存在目标伪装成提交回执。
    pub fn confirm_item_unchanged(
        &mut self,
    ) -> Result<TransferCommitSummary, TransferCommitTransitionError> {
        if self.phase != CommitPhase::ItemInProgress {
            return Err(TransferCommitTransitionError::NoItemInProgress);
        }
        self.phase = if self.summary.is_empty() {
            CommitPhase::Ready
        } else {
            CommitPhase::BetweenItems
        };
        Ok(self.summary)
    }

    /// 撤销尚未送达后端的提交许可；只有控制通道能证明调用方未收到许可时才可调用。
    pub fn abort_unacknowledged_item(
        &mut self,
    ) -> Result<TransferCommitSummary, TransferCommitTransitionError> {
        if self.phase != CommitPhase::ItemInProgress {
            return Err(TransferCommitTransitionError::NoItemInProgress);
        }
        self.phase = if self.summary.is_empty() {
            CommitPhase::Ready
        } else {
            CommitPhase::BetweenItems
        };
        Ok(self.summary)
    }

    /// 后端确认所有目标项均已回执后进入最终保护区，允许最终 receipt 与迟到取消竞态收敛。
    pub fn finish_receipts(
        &mut self,
    ) -> Result<TransferCommitSummary, TransferCommitTransitionError> {
        match self.phase {
            CommitPhase::BetweenItems if !self.summary.is_empty() => {
                self.phase = CommitPhase::Finalized;
                Ok(self.summary)
            }
            CommitPhase::Ready | CommitPhase::BetweenItems => {
                Err(TransferCommitTransitionError::NoConfirmedReceipt)
            }
            CommitPhase::ItemInProgress => {
                Err(TransferCommitTransitionError::ItemAlreadyInProgress)
            }
            CommitPhase::Finalized => Err(TransferCommitTransitionError::AlreadyFinalized),
            CommitPhase::OutcomeUnknown => Err(TransferCommitTransitionError::OutcomeUnknown),
        }
    }

    /// 完整目录遍历没有产生目标改动时结束任务，不伪造 receipt 或额外修改目标属性。
    pub fn finish_no_changes(
        &mut self,
    ) -> Result<TransferCommitSummary, TransferCommitTransitionError> {
        if self.cancel_requested {
            return Err(TransferCommitTransitionError::CancellationRequested);
        }
        match self.phase {
            CommitPhase::Ready if self.summary.is_empty() => {
                self.phase = CommitPhase::Finalized;
                Ok(self.summary)
            }
            CommitPhase::ItemInProgress => {
                Err(TransferCommitTransitionError::ItemAlreadyInProgress)
            }
            CommitPhase::Finalized => Err(TransferCommitTransitionError::AlreadyFinalized),
            CommitPhase::OutcomeUnknown => Err(TransferCommitTransitionError::OutcomeUnknown),
            CommitPhase::Ready | CommitPhase::BetweenItems => {
                Err(TransferCommitTransitionError::NoChangesNotAllowed)
            }
        }
    }

    /// 提交期间无法取得回执时结束重试资格，并保留此前明确完成项目的统计。
    pub fn mark_outcome_unknown(
        &mut self,
    ) -> Result<TransferCommitSummary, TransferCommitTransitionError> {
        if self.phase != CommitPhase::ItemInProgress {
            return Err(TransferCommitTransitionError::NoItemInProgress);
        }
        self.phase = CommitPhase::OutcomeUnknown;
        Ok(self.summary)
    }

    /// 项间允许取消，提交回执未决、最终结果等待及结果未知状态不能被取消覆盖。
    pub const fn cancellation_is_deferred(self) -> bool {
        matches!(
            self.phase,
            CommitPhase::ItemInProgress | CommitPhase::Finalized | CommitPhase::OutcomeUnknown
        )
    }

    /// 查询取消是否已提出；后端据此在项间结束而不丢弃当前原子提交。
    pub const fn cancellation_requested(self) -> bool {
        self.cancel_requested
    }

    /// 只在目标操作尚未取得明确回执时返回 true，供应用队列仲裁取消时机。
    pub const fn item_in_progress(self) -> bool {
        matches!(self.phase, CommitPhase::ItemInProgress)
    }

    /// 已收到上一项回执且还没有最终屏障时可取消或开始下一项。
    pub const fn between_items(self) -> bool {
        matches!(self.phase, CommitPhase::BetweenItems)
    }

    /// 只有没有任何已提交项且当前结果不确定性已排除时才可安全自动重试。
    pub const fn can_retry_safely(self) -> bool {
        !self.cancel_requested
            && !matches!(
                self.phase,
                CommitPhase::ItemInProgress | CommitPhase::Finalized | CommitPhase::OutcomeUnknown
            )
            && self.summary.is_empty()
    }

    /// 读取已确认目标结果，失败或取消状态也可继续投影这一摘要。
    pub const fn summary(self) -> TransferCommitSummary {
        self.summary
    }

    /// 结果屏障完成后不允许再次提交，保证成功与晚到取消有唯一裁决。
    pub const fn is_finalized(self) -> bool {
        matches!(self.phase, CommitPhase::Finalized)
    }

    /// 未确认项结果未知时，状态机拒绝自动重试并要求目标 readback 或人工检查。
    pub const fn is_outcome_unknown(self) -> bool {
        matches!(self.phase, CommitPhase::OutcomeUnknown)
    }
}
