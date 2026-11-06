//! 列宽重排只调整可见折行，不把历史空行变成 Shell 正文上方的额外留白。
//!
//! @author kongweiguang

use kerminal_domain::{identity::SessionId, sessions::TerminalSize};
use kerminal_terminal::{
    engine::{TerminalEngine, TerminalLimits, TerminalResizePolicy},
    snapshot::{PlainTextSnapshotRequest, RenderSnapshot, RenderSnapshotRequest},
};

const SHELL_OUTPUT: &[u8] = b"root@pkuai01:~# sss\r\nCommand 'sss' not found, but there are 17 similar ones.\r\nroot@pkuai01:~# \r\nroot@pkuai01:~# \r\nroot@pkuai01:~# ";

/// 固定窄网格让同一报错恰好折成三行，避免依赖机器字体或窗口尺寸。
fn narrow_terminal() -> TerminalEngine {
    TerminalEngine::new(
        SessionId::new(),
        TerminalSize::new(20, 37, 0, 0).unwrap(),
        TerminalLimits::default(),
    )
    .unwrap()
}

/// 用真实换行建立空历史，避免直接改私有网格而绕过 parser 和快照缓存。
fn shell_with_blank_history(leading_blank_line: bool) -> TerminalEngine {
    let mut terminal = narrow_terminal();
    terminal.process_output("\r\n".repeat(40).as_bytes());
    terminal.process_output(b"\x1b[H");
    if leading_blank_line {
        terminal.process_output(b"\r\n");
    }
    terminal.process_output(SHELL_OUTPUT);
    terminal
}

/// 只去掉行尾的网格填充，保留行首与整行空白，以免断言自行掩盖留白。
fn row_text(snapshot: &RenderSnapshot, row: usize) -> String {
    snapshot.rows[row]
        .runs
        .iter()
        .map(|run| run.text.as_ref())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// 同时回读历史与正文，确认修复没有通过丢弃命令或历史换取视觉结果。
fn all_text(terminal: &TerminalEngine) -> String {
    terminal
        .plaintext_snapshot(PlainTextSnapshotRequest {
            include_scrollback: true,
            max_lines: 512,
            max_bytes: Some(64 * 1024),
        })
        .text
        .replace('\n', "")
}

/// 折行合并释放的行应补在底部；原历史空行仍在历史中，不出现在 Shell 首行之前。
#[test]
fn growing_columns_does_not_pull_blank_history_above_shell_output() {
    let mut terminal = shell_with_blank_history(false);
    let before = terminal.render_snapshot(RenderSnapshotRequest::default());
    let text_before = all_text(&terminal);
    assert_eq!(row_text(&before, 0), "root@pkuai01:~# sss");
    assert!(before.history_size >= 2);

    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());

    assert_eq!(row_text(&after, 0), "root@pkuai01:~# sss");
    assert_eq!(
        row_text(&after, 1),
        "Command 'sss' not found, but there are 17 similar ones."
    );
    assert_eq!(after.history_size, before.history_size);
    assert_eq!(after.cursor.row, before.cursor.row - 2);
    assert_eq!(after.cursor.column, before.cursor.column);
    assert_eq!(all_text(&terminal), text_before);
}

/// Shell 主动输出的首行空白必须保留，不能用统一 trim 修掉历史拉回造成的空白。
#[test]
fn growing_columns_keeps_explicit_visible_blank_lines() {
    let mut terminal = shell_with_blank_history(true);
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());

    assert_eq!(row_text(&after, 0), "");
    assert_eq!(row_text(&after, 1), "root@pkuai01:~# sss");
}

/// 没有历史时继续按折行内容移动光标，不为新门禁创建额外历史或填充。
#[test]
fn growing_columns_without_history_keeps_the_first_line() {
    let mut terminal = narrow_terminal();
    terminal.process_output(SHELL_OUTPUT);
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(row_text(&after, 0), "root@pkuai01:~# sss");
    assert_eq!(after.history_size, 0);
    assert_eq!((after.cursor.row, after.cursor.column), (4, 16));
}

/// 有内容的历史沿用上游拉回行为，不能把旧输出当成空白抹掉。
#[test]
fn growing_columns_preserves_nonblank_history_rows() {
    let mut terminal = narrow_terminal();
    let history = (0..40)
        .map(|index| format!("H{index:02}\r\n"))
        .collect::<String>();
    terminal.process_output(history.as_bytes());
    terminal.process_output(b"\x1b[H");
    terminal.process_output(SHELL_OUTPUT);
    let before = all_text(&terminal);
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(row_text(&after, 0), "H02");
    assert_eq!(row_text(&after, 1), "H03");
    assert_eq!(all_text(&terminal), before);
}

/// 带背景色的空格也是终端内容，严格默认 cell 门禁不能把它作为无意义留白归还历史。
#[test]
fn growing_columns_preserves_styled_blank_history() {
    let mut terminal = narrow_terminal();
    terminal.process_output("\x1b[44m \r\n".repeat(40).as_bytes());
    terminal.process_output(b"\x1b[0m\x1b[H");
    terminal.process_output(SHELL_OUTPUT);
    terminal.scroll(2);
    let history_row = terminal.render_snapshot(RenderSnapshotRequest::default());
    let expected_style = history_row.rows[0].runs[0].style;
    terminal.scroll(-2);
    let before_history = terminal
        .render_snapshot(RenderSnapshotRequest::default())
        .history_size;
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(after.rows[0].runs[0].style, expected_style);
    assert_eq!(after.history_size, before_history - 2);
}

/// 用户正在阅读历史时保留上游视口，不自动把历史中的空行赶回不可见区。
#[test]
fn growing_columns_does_not_reposition_a_scrolled_viewport() {
    let mut terminal = shell_with_blank_history(false);
    terminal.scroll(1);
    let before = all_text(&terminal);
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(after.scrollback_offset, 1);
    assert_eq!(row_text(&after, 0), "");
    assert_eq!(after.history_size, 2);
    assert_eq!(all_text(&terminal), before);
}

/// 保存光标遵循 DEC 屏幕坐标；不能把活动光标的内容移动量误施加到另一个保存点。
#[test]
fn growing_columns_preserves_saved_cursor_screen_coordinates() {
    let mut terminal = shell_with_blank_history(false);
    terminal.process_output(b"\x1b[5;7H\x1b7\x1b[7;17H");
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    terminal.process_output(b"\x1b8");
    assert_eq!(terminal.diagnostic_cursor_position(), (4, 6));
}

/// 备用屏不参与列重排，后台 primary 仍按同一安全规则保留文本与历史。
#[test]
fn growing_columns_keeps_alternate_screen_and_inactive_primary_separate() {
    let mut terminal = shell_with_blank_history(false);
    let primary_text = all_text(&terminal);
    terminal.process_output(b"\x1b[?1049h\x1b[Halt-screen");
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let alternate = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert!(alternate.modes.alternate_screen);
    assert_eq!(row_text(&alternate, 0), "alt-screen");
    terminal.process_output(b"\x1b[?1049l");
    let primary = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(row_text(&primary, 0), "root@pkuai01:~# sss");
    assert_eq!(all_text(&terminal), primary_text);
}

/// 多次往返不累积顶部空行或损失历史文本，覆盖光标所在行也参与合并的情况。
#[test]
fn repeated_column_reflow_keeps_shell_text_and_does_not_accumulate_blank_rows() {
    let mut terminal = shell_with_blank_history(false);
    let before = all_text(&terminal);
    for width in [104, 20, 104, 20, 104] {
        terminal
            .resize(TerminalSize::new(width, 37, 0, 0).unwrap())
            .unwrap();
        let after = terminal.render_snapshot(RenderSnapshotRequest::default());
        assert!(!row_text(&after, 0).is_empty(), "width {width}");
        assert_eq!(all_text(&terminal), before, "width {width}");
    }
}

/// 首行是历史软折行的续行时沿用原路径；与历史阅读后回底的同流结果逐格一致。
#[test]
fn growing_columns_keeps_soft_wrap_across_the_history_boundary() {
    let mut active = narrow_terminal();
    let mut reading = narrow_terminal();
    let wrapped_spaces = " ".repeat(20 * 42);
    for terminal in [&mut active, &mut reading] {
        terminal.process_output(wrapped_spaces.as_bytes());
        terminal.process_output(b"\x1b[H");
        terminal.process_output(SHELL_OUTPUT);
    }
    reading.scroll(1);
    for terminal in [&mut active, &mut reading] {
        terminal
            .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
            .unwrap();
        terminal.scroll(-i32::MAX);
    }
    let active = active.render_snapshot(RenderSnapshotRequest::default());
    let reading = reading.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(active.rows, reading.rows);
    assert_eq!(active.cursor, reading.cursor);
    assert_eq!(active.history_size, reading.history_size);
}

/// 当前光标自身所在折行被合并时仍落在原文字末尾，不能因尾部截断重复移动。
#[test]
fn growing_columns_reflows_the_cursor_with_its_own_line() {
    let mut terminal = narrow_terminal();
    terminal.process_output("\r\n".repeat(40).as_bytes());
    terminal.process_output(b"\x1b[Hroot@pkuai01:~# sss\r\n");
    let message = b"Command 'sss' not found, but there are 17 similar ones.";
    terminal.process_output(message);
    let history = terminal
        .render_snapshot(RenderSnapshotRequest::default())
        .history_size;
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(row_text(&after, 0), "root@pkuai01:~# sss");
    assert_eq!(
        (after.cursor.row, after.cursor.column),
        (1, message.len() as u16)
    );
    assert_eq!(after.history_size, history);
}

/// 混合历史中只归还最前面的默认空行；逐行比较同时保护真实历史和 Shell 的硬换行。
#[test]
fn growing_columns_preserves_mixed_history_and_visible_blank_line_order() {
    let mut terminal = narrow_terminal();
    for index in 0..40 {
        if index == 2 {
            terminal.process_output(b"\r\n");
        } else {
            terminal.process_output(format!("H{index:02}\r\n").as_bytes());
        }
    }
    terminal.process_output(b"\x1b[H\x1b[2K\r\n");
    terminal.process_output(SHELL_OUTPUT);
    terminal
        .resize(TerminalSize::new(104, 37, 0, 0).unwrap())
        .unwrap();
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());
    let first_rows: Vec<_> = (0..7).map(|row| row_text(&after, row)).collect();
    assert_eq!(
        first_rows,
        [
            "H03",
            "",
            "root@pkuai01:~# sss",
            "Command 'sss' not found, but there are 17 similar ones.",
            "root@pkuai01:~#",
            "root@pkuai01:~#",
            "root@pkuai01:~#",
        ]
    );
    assert_eq!(after.history_size, 3);
    terminal.scroll(3);
    let history = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(
        (0..4)
            .map(|row| row_text(&history, row))
            .collect::<Vec<_>>(),
        ["H00", "H01", "", "H03"]
    );
}

/// 行列同时变化仍遵守各 transport 的扩行策略，历史满载或不足两行时不能越界或增生。
#[test]
fn simultaneous_row_and_column_growth_respects_saturated_history_limits() {
    for policy in [TerminalResizePolicy::Generic, TerminalResizePolicy::ConPty] {
        for history_limit in [1, 3] {
            for rows in [35, 39] {
                let mut terminal = TerminalEngine::new_with_resize_policy(
                    SessionId::new(),
                    TerminalSize::new(20, 37, 0, 0).unwrap(),
                    TerminalLimits {
                        max_scrollback_lines: history_limit,
                        ..TerminalLimits::default()
                    },
                    policy,
                )
                .unwrap();
                terminal.process_output("\r\n".repeat(40).as_bytes());
                terminal.process_output(b"\x1b[H");
                terminal.process_output(SHELL_OUTPUT);
                let before = all_text(&terminal);
                terminal
                    .resize(TerminalSize::new(104, rows, 0, 0).unwrap())
                    .unwrap();
                let after = terminal.render_snapshot(RenderSnapshotRequest::default());
                let pulled_by_row_growth = if rows > 37 && policy == TerminalResizePolicy::Generic {
                    history_limit.min(2)
                } else {
                    0
                };
                assert_eq!(
                    row_text(&after, pulled_by_row_growth),
                    "root@pkuai01:~# sss",
                    "policy={policy:?}, history={history_limit}, rows={rows}"
                );
                assert_eq!(after.history_size, history_limit - pulled_by_row_growth);
                assert_eq!(after.cursor.row as usize, 4 + pulled_by_row_growth);
                assert_eq!(all_text(&terminal), before);
            }
        }
    }
}
