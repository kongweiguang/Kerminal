//! 右栏预览的 Fill 门禁与 trim 后命令正文规则。
//!
//! @author kongweiguang

use super::{terminal_fill_command_is_multiline, terminal_fill_command_is_valid};

/// 首尾空白按应用层 trim 语义移除，行内 Tab 不改变单行命令的可填入状态。
///
/// @author kongweiguang
#[test]
fn terminal_fill_gate_matches_trimmed_single_line_rules() {
    assert!(terminal_fill_command_is_valid("  echo\tready  \r\n"));
    assert!(!terminal_fill_command_is_valid(" \t "));
    assert!(!terminal_fill_command_is_valid("echo first\necho second"));
    assert!(!terminal_fill_command_is_valid("echo first\r\necho second"));
}

/// 仅 trim 后正文的 CR/LF 触发与 main 相同的多行提示。
///
/// @author kongweiguang
#[test]
fn multiline_hint_tracks_only_trimmed_command_body() {
    assert!(terminal_fill_command_is_multiline(
        "echo first\necho second"
    ));
    assert!(!terminal_fill_command_is_multiline("\r\n echo ready \n"));
    assert!(!terminal_fill_command_is_multiline("echo\tready"));
}
