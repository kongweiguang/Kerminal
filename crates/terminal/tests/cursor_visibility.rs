//! 验证 ConPTY 光标可见性跟踪跨分包、幂等和重置语义，避免合批误吞普通输出。
//!
//! @author kongweiguang

use kerminal_terminal::cursor_visibility::CursorVisibilityTracker;

/// 隐藏和显示分两次读取时，片段边界必须分别报告新隐藏周期和恢复可见。
///
/// @author kongweiguang
#[test]
fn hide_and_show_across_reads_report_both_boundaries() {
    let mut tracker = CursorVisibilityTracker::default();
    let hide = tracker.advance(b"\x1b[m\x1b[?25l");
    assert!(hide.newly_hidden);
    assert!(!hide.after_visible);
    let show = tracker.advance(b"\x1b[93m\x1b[1;20Hab \x08\x1b[?25h");
    assert!(!show.newly_hidden);
    assert!(show.after_visible);
}

/// 每个分包位置都能连续解析，最终状态一致且未闭合序列不会提前改变可见性。
///
/// @author kongweiguang
#[test]
fn transitions_survive_every_split() {
    let wire = b"\x1b[m\x1b[?25lredraw\x1b[?25h";
    for split in 0..=wire.len() {
        let mut tracker = CursorVisibilityTracker::default();
        tracker.advance(&wire[..split]);
        let second = tracker.advance(&wire[split..]);
        assert!(second.after_visible, "split {split}");
        assert!(!second.newly_hidden, "split {split}");
    }
}

/// 普通输出没有可见性切换，结尾保持可见且不产生隐藏周期。
///
/// @author kongweiguang
#[test]
fn ordinary_output_keeps_cursor_visible() {
    let mut tracker = CursorVisibilityTracker::default();
    let change = tracker.advance(b"hello \x1b[31mworld\x1b[m");
    assert!(change.after_visible);
    assert!(!change.newly_hidden);
}

/// 重复隐藏或重复显示保持幂等，不会让上层误以为出现新的等待周期。
///
/// @author kongweiguang
#[test]
fn repeated_visibility_requests_are_idempotent() {
    let mut tracker = CursorVisibilityTracker::default();
    let hidden = tracker.advance(b"\x1b[?25l\x1b[?25l\x1b[?25l");
    assert!(hidden.newly_hidden);
    assert!(!hidden.after_visible);
    let still_hidden = tracker.advance(b"\x1b[?25l");
    assert!(!still_hidden.newly_hidden);
    assert!(!still_hidden.after_visible);
    let shown = tracker.advance(b"\x1b[?25h\x1b[?25h");
    assert!(!shown.newly_hidden);
    assert!(shown.after_visible);
}

/// 超时释放后光标仍隐藏，同一片段里的 show→hide 必须重新报告新隐藏周期。
///
/// @author kongweiguang
#[test]
fn show_then_hide_after_hidden_starts_new_cycle() {
    let mut tracker = CursorVisibilityTracker::default();
    assert!(tracker.advance(b"\x1b[?25l").newly_hidden);
    let change = tracker.advance(b"\x1b[?25h\x1b[?25l");
    assert!(change.newly_hidden);
    assert!(!change.after_visible);
    // 新周期内再次隐藏仍属重复，不能刷新等待上限。
    let repeated = tracker.advance(b"\x1b[?25l");
    assert!(!repeated.newly_hidden);
    assert!(!repeated.after_visible);
}

/// 从可见开始的一次 hide→show→hide 同样要在结束隐藏时报告新周期。
///
/// @author kongweiguang
#[test]
fn hide_show_hide_reports_unclosed_final_hide() {
    let mut tracker = CursorVisibilityTracker::default();
    let change = tracker.advance(b"\x1b[?25l\x1b[?25h\x1b[?25l");
    assert!(change.newly_hidden);
    assert!(!change.after_visible);
}

/// OSC/DCS 中的同样文本属于字符串正文，不能被当成 CSI 私有模式。
///
/// @author kongweiguang
#[test]
fn string_control_payloads_do_not_change_visibility() {
    let mut tracker = CursorVisibilityTracker::default();
    let change = tracker.advance(b"\x1b]0;?25l\x07\x1bP?25l\x1b\\");
    assert!(!change.newly_hidden);
    assert!(change.after_visible);
}

/// RIS 会恢复默认可见光标；隐藏期间的 RIS 结束等待。
///
/// @author kongweiguang
#[test]
fn reset_escape_restores_visibility() {
    let mut tracker = CursorVisibilityTracker::default();
    assert!(!tracker.advance(b"\x1b[?25l").after_visible);
    let change = tracker.advance(b"\x1bc");
    assert!(!change.newly_hidden);
    assert!(change.after_visible);
}

/// 与 vte ansi 一致，私有模式 25 可以和其他参数同列，不能只匹配单参数序列。
///
/// @author kongweiguang
#[test]
fn multi_parameter_private_modes_include_cursor_visibility() {
    let mut tracker = CursorVisibilityTracker::default();
    let unrelated = tracker.advance(b"\x1b[?1049h");
    assert!(unrelated.after_visible);
    let hidden = tracker.advance(b"\x1b[?25;1l");
    assert!(hidden.newly_hidden);
    let shown = tracker.advance(b"\x1b[?25;1049h");
    assert!(shown.after_visible);
}
