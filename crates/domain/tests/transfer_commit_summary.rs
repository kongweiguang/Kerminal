//! 已确认提交摘要只反映目标项回执，和传输/暂存进度分开统计。
//!
//! @author kongweiguang

#[path = "../src/files/transfer_outcome.rs"]
mod transfer_outcome;

use transfer_outcome::{TransferCommitSummary, TransferCommitSummaryOverflow};

/// 目录创建和空文件都计入已提交项，只有文件内容增加字节数。
#[test]
fn confirmed_entries_and_bytes_are_distinct() {
    let mut summary = TransferCommitSummary::empty();
    summary
        .record_confirmed_entry(0)
        .expect("directory receipt");
    summary
        .record_confirmed_entry(0)
        .expect("empty-file receipt");
    summary
        .record_confirmed_entry(8192)
        .expect("file commit receipt");

    assert_eq!(summary.committed_entries(), 3);
    assert_eq!(summary.committed_bytes(), 8192);
    assert!(!summary.is_empty());
}

/// 摘要合并只增加已明确回执的计数。
#[test]
fn merge_adds_only_confirmed_summaries() {
    let mut previous = TransferCommitSummary::empty();
    previous.record_confirmed_entry(5).expect("first file");
    let mut current = TransferCommitSummary::empty();
    current.record_confirmed_entry(7).expect("second file");

    previous.merge(current).expect("counts fit");
    assert_eq!(previous.committed_entries(), 2);
    assert_eq!(previous.committed_bytes(), 12);
}

/// 溢出不能饱和后伪装成精确的部分提交结果。
#[test]
fn overflow_is_reported_without_saturating() {
    let mut summary = TransferCommitSummary::empty();
    summary
        .record_confirmed_entry(u64::MAX)
        .expect("maximum byte count");
    assert_eq!(
        summary.record_confirmed_entry(1),
        Err(TransferCommitSummaryOverflow),
    );
    assert_eq!(summary.committed_entries(), 1);
    assert_eq!(summary.committed_bytes(), u64::MAX);
}
