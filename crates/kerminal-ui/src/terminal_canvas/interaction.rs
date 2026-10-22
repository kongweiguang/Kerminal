//! 终端指针交互的 cell 命中映射，不从 UTF-8 文本宽度猜测终端列。
//!
//! @author kongweiguang

use crate::features::terminal_tools::selection::ViewportCell;
use kerminal_terminal::snapshot::TerminalRow;

/// 将列夹在网格内并要求 viewport 行真实存在，稀疏 damage 快照不猜测绝对行号。
///
/// @author kongweiguang
pub(crate) fn clamp_viewport_cell(
    rows: &[TerminalRow],
    columns: u16,
    cell: ViewportCell,
) -> Option<ViewportCell> {
    if columns == 0 {
        return None;
    }

    let row = rows.iter().find(|row| row.viewport_row == cell.row)?;
    Some(ViewportCell {
        row: row.viewport_row,
        column: cell.column.min(columns.saturating_sub(1)),
    })
}

/// 普通单击只记录清除意图，延迟到 MouseUp 核验后提交，避免覆盖随后形成的新选区。
///
/// @author kongweiguang
pub(crate) fn should_defer_selection_clear_on_mouse_down(
    plain_single_left_click: bool,
    has_selection: bool,
) -> bool {
    plain_single_left_click && has_selection
}

/// 只有按下与抬起都保持显式修饰键且单击时，才把终端点击解释为打开链接。
///
/// @author kongweiguang
pub(crate) fn allows_osc8_click(
    modifier_was_held_on_press: bool,
    modifier_is_held_on_release: bool,
    click_count: usize,
) -> bool {
    modifier_was_held_on_press && modifier_is_held_on_release && click_count == 1
}
