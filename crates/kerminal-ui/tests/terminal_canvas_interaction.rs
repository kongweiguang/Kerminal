//! 终端画布鼠标命中与稀疏快照契约测试。
//!
//! @author kongweiguang

#[path = "../src/terminal_canvas/interaction.rs"]
mod interaction;

mod features {
    pub mod terminal_tools {
        pub mod selection {
            #[derive(Clone, Copy, Debug, Eq, PartialEq)]
            pub struct ViewportCell {
                pub row: u16,
                pub column: u16,
            }
        }
    }
}

use std::sync::Arc;

use features::terminal_tools::selection::ViewportCell;
use interaction::{
    allows_osc8_click, clamp_viewport_cell, should_defer_selection_clear_on_mouse_down,
};
use kerminal_terminal::snapshot::TerminalRow;

/// 夹取只依赖行身份，链接边界交由生产 links 命中测试覆盖，避免这里维护另一套映射。
///
/// @author kongweiguang
fn row(viewport_row: u16) -> TerminalRow {
    TerminalRow {
        viewport_row,
        absolute_line: 100 + i64::from(viewport_row),
        wrapped: false,
        runs: Arc::from([]),
    }
}

/// 稀疏快照只接受实际存在的视口行，不能把缺失行猜成邻近的绝对行。
///
/// @author kongweiguang
#[test]
fn pointer_cell_clamps_column_but_requires_exact_viewport_row() {
    let rows = [row(2), row(8)];

    assert_eq!(
        clamp_viewport_cell(&rows, 5, ViewportCell { row: 2, column: 9 }),
        Some(ViewportCell { row: 2, column: 4 })
    );
    assert_eq!(
        clamp_viewport_cell(&rows, 5, ViewportCell { row: 5, column: 1 }),
        None
    );
    assert_eq!(
        clamp_viewport_cell(&rows, 0, ViewportCell { row: 2, column: 1 }),
        None
    );
}

/// 普通点击与拖动不打开链接；显式修饰键必须覆盖完整的单击手势。
///
/// @author kongweiguang
#[test]
fn osc8_links_require_modifier_for_press_and_release_and_single_click() {
    assert!(!allows_osc8_click(false, false, 1));
    assert!(!allows_osc8_click(true, false, 1));
    assert!(!allows_osc8_click(false, true, 1));
    assert!(!allows_osc8_click(true, true, 2));
    assert!(allows_osc8_click(true, true, 1));
}

/// 普通未修饰单击不论点击字形或空白都延迟清除；TUI 由另一条鼠标路由处理。
///
/// @author kongweiguang
#[test]
fn ordinary_mouse_down_clear_intent_requires_existing_selection_and_plain_click() {
    assert!(should_defer_selection_clear_on_mouse_down(true, true));
    assert!(!should_defer_selection_clear_on_mouse_down(true, false));
    assert!(!should_defer_selection_clear_on_mouse_down(false, true));
}
