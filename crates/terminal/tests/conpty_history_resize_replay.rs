//! 固定真实 ConPTY 重画序列，防止纯网格扩行测试漏掉随后覆盖历史的行为。
//!
//! @author kongweiguang

use kerminal_domain::{identity::SessionId, sessions::TerminalSize};
use kerminal_terminal::{
    engine::{TerminalEngine, TerminalLimits, TerminalResizePolicy},
    snapshot::PlainTextSnapshotRequest,
};

const INITIAL: &[u8] = include_bytes!("fixtures/conpty-history-initial.vt");
const EXPANDED: &[u8] = include_bytes!("fixtures/conpty-history-expanded.vt");
const RETURNED: &[u8] = include_bytes!("fixtures/conpty-history-returned.vt");

/// 按捕获时的初始尺寸构造引擎，显式 policy 让失败对照与实际修复消费相同字节。
///
/// @author kongweiguang
fn engine(policy: TerminalResizePolicy) -> TerminalEngine {
    TerminalEngine::new_with_resize_policy(
        SessionId::new(),
        TerminalSize::new(52, 15, 0, 0).expect("captured initial dimensions"),
        TerminalLimits::default(),
        policy,
    )
    .expect("bounded captured terminal")
}

/// 从完整且未截断的历史中提取编号，比较顺序同时检查丢失、重复和越界。
///
/// @author kongweiguang
fn history_ids(terminal: &TerminalEngine) -> Vec<u16> {
    let snapshot = terminal.plaintext_snapshot(PlainTextSnapshotRequest {
        include_scrollback: true,
        max_lines: 2000,
        max_bytes: Some(64 * 1024),
    });
    assert!(!snapshot.truncated);
    snapshot
        .text
        .lines()
        .filter_map(|line| line.strip_prefix("R40-IDLE-"))
        .filter_map(|tail| tail.get(..3)?.parse().ok())
        .collect()
}

/// resize 必须先于该尺寸触发的 PTY 重画，避免测试只覆盖静态网格而遗漏真实覆盖顺序。
///
/// @author kongweiguang
fn resize_and_repaint(terminal: &mut TerminalEngine, columns: u16, rows: u16, bytes: &[u8]) {
    terminal
        .resize(TerminalSize::new(columns, rows, 0, 0).expect("captured resize dimensions"))
        .expect("captured resize");
    terminal.process_output(bytes);
}

/// Windows 11 ConPTY 的重画不重发 145/146；专用策略须在扩行、缩回后仍保留全部 Unicode 编号行。
///
/// @author kongweiguang
#[test]
fn captured_conpty_repaint_preserves_all_history_ids() {
    let mut terminal = engine(TerminalResizePolicy::ConPty);
    terminal.process_output(INITIAL);
    let expected = (1..=160).collect::<Vec<_>>();
    assert_eq!(history_ids(&terminal), expected);

    resize_and_repaint(&mut terminal, 64, 17, EXPANDED);
    assert_eq!(history_ids(&terminal), expected);
    resize_and_repaint(&mut terminal, 52, 15, RETURNED);
    assert_eq!(history_ids(&terminal), expected);
}

/// 同字节失败对照固定问题成因；通用策略仍适用于 SSH/Unix，但不能用于这个 ConPTY 重画契约。
///
/// @author kongweiguang
#[test]
fn captured_conpty_repaint_exposes_generic_history_overwrite() {
    let mut terminal = engine(TerminalResizePolicy::Generic);
    terminal.process_output(INITIAL);
    assert_eq!(history_ids(&terminal), (1..=160).collect::<Vec<_>>());

    resize_and_repaint(&mut terminal, 64, 17, EXPANDED);
    let expected = (1..=160)
        .filter(|id| !matches!(id, 145 | 146))
        .collect::<Vec<_>>();
    assert_eq!(history_ids(&terminal), expected);
    resize_and_repaint(&mut terminal, 52, 15, RETURNED);
    assert_eq!(history_ids(&terminal), expected);
}
