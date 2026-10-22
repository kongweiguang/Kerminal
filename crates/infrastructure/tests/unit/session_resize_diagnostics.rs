//! Terminal resize diagnostic parser and generation tests.
//
// @author kongweiguang

use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    MAX_RESIZE_DIAGNOSTIC_READER_CHUNKS, claim_resize_cpr_diagnostic_generation,
    claim_resize_reader_diagnostic_chunk, parse_cursor_position_report,
    reset_resize_reader_diagnostic_budget,
};

/// CPR 诊断只接受 engine 完整生成的一基行列回复，避免把其它 VT 响应当作光标证据。
///
/// @author kongweiguang
#[test]
fn cursor_position_report_accepts_only_complete_cpr() {
    assert_eq!(
        parse_cursor_position_report(b"\x1b[31;112R"),
        Some((31, 112))
    );
    assert_eq!(parse_cursor_position_report(b"\x1b[1;1R"), Some((1, 1)));
}

/// 拒绝不完整、扩展参数、溢出和零坐标，避免诊断误记或截断协议数据。
///
/// @author kongweiguang
#[test]
fn cursor_position_report_rejects_malformed_or_other_vt_sequences() {
    for bytes in [
        &b"\x1b[31;112"[..],
        &b"\x1b[31;112Rx"[..],
        &b"\x1b[31;112;4R"[..],
        &b"\x1b[;112R"[..],
        &b"\x1b[0;112R"[..],
        &b"\x1b[31;0R"[..],
        &b"\x1b[65536;112R"[..],
        &b"\x1b[31;65536R"[..],
        &b"\x1b[?31;112R"[..],
        &b"\x1b]31;112R"[..],
    ] {
        assert_eq!(parse_cursor_position_report(bytes), None, "{bytes:?}");
    }
}

/// CPR 只消费当前 resize 的一次性关联标记；旧回复不得消费更新后的 resize generation。
///
/// @author kongweiguang
#[test]
fn cursor_position_report_is_claimed_once_for_its_resize_generation() {
    let pending = AtomicU64::new(7);
    assert!(claim_resize_cpr_diagnostic_generation(&pending, 7));
    assert!(!claim_resize_cpr_diagnostic_generation(&pending, 7));

    pending.store(8, Ordering::Release);
    assert!(!claim_resize_cpr_diagnostic_generation(&pending, 7));
    assert!(claim_resize_cpr_diagnostic_generation(&pending, 8));
    assert!(!claim_resize_cpr_diagnostic_generation(&pending, 0));
}

/// reader trace 每次 resize 都可重置，但单代最多采样 16 个 chunk，且耗尽不欠账。
///
/// @author kongweiguang
#[test]
fn ssh_reader_resize_diagnostic_budget_is_bounded_and_resettable() {
    let budget = AtomicU64::new(0);
    assert!(!claim_resize_reader_diagnostic_chunk(&budget));
    reset_resize_reader_diagnostic_budget(&budget);
    for _ in 0..MAX_RESIZE_DIAGNOSTIC_READER_CHUNKS {
        assert!(claim_resize_reader_diagnostic_chunk(&budget));
    }
    assert!(!claim_resize_reader_diagnostic_chunk(&budget));
    assert_eq!(budget.load(Ordering::Acquire), 0);

    reset_resize_reader_diagnostic_budget(&budget);
    assert!(claim_resize_reader_diagnostic_chunk(&budget));
    assert_eq!(
        budget.load(Ordering::Acquire),
        MAX_RESIZE_DIAGNOSTIC_READER_CHUNKS - 1
    );
}
