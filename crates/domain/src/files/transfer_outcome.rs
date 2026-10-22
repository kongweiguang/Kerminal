//! 目录传输在多项独立提交时的已确认结果摘要；不把暂存字节误作目标已改变。
//!
//! @author kongweiguang

/// 只统计获得明确提交回执的目标项；不确定回执必须由调用方另报 CommitUnknown。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferCommitSummary {
    committed_entries: u64,
    committed_bytes: u64,
}

/// 计数溢出表示调用方生命周期或计数来源异常，不能饱和后伪造精确摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferCommitSummaryOverflow;

impl TransferCommitSummary {
    /// 创建空摘要，适用于尚未进入任何目标提交的任务。
    pub const fn empty() -> Self {
        Self {
            committed_entries: 0,
            committed_bytes: 0,
        }
    }

    /// 在单项提交已明确完成后递增计数；目录项传入零字节，文件项传入其确认大小。
    pub fn record_confirmed_entry(
        &mut self,
        committed_bytes: u64,
    ) -> Result<(), TransferCommitSummaryOverflow> {
        let entries = self
            .committed_entries
            .checked_add(1)
            .ok_or(TransferCommitSummaryOverflow)?;
        let bytes = self
            .committed_bytes
            .checked_add(committed_bytes)
            .ok_or(TransferCommitSummaryOverflow)?;
        self.committed_entries = entries;
        self.committed_bytes = bytes;
        Ok(())
    }

    /// 合并另一个已确认摘要，用于把多个传输阶段的终态信息统一投影。
    pub fn merge(&mut self, other: Self) -> Result<(), TransferCommitSummaryOverflow> {
        let entries = self
            .committed_entries
            .checked_add(other.committed_entries)
            .ok_or(TransferCommitSummaryOverflow)?;
        let bytes = self
            .committed_bytes
            .checked_add(other.committed_bytes)
            .ok_or(TransferCommitSummaryOverflow)?;
        self.committed_entries = entries;
        self.committed_bytes = bytes;
        Ok(())
    }

    /// 返回已经明确提交的文件和目录项数量，不包含 staging 文件或未知回执。
    pub const fn committed_entries(self) -> u64 {
        self.committed_entries
    }

    /// 返回上述已提交文件的字节数；目录项本身不增加字节统计。
    pub const fn committed_bytes(self) -> u64 {
        self.committed_bytes
    }

    /// 空摘要可安全映射为尚无已确认目标变更。
    pub const fn is_empty(self) -> bool {
        self.committed_entries == 0 && self.committed_bytes == 0
    }
}
