//! 查找面板模式转换回归。
//!
//! @author kongweiguang

use super::SearchPanelMode;

/// Ctrl/Cmd+F 从关闭或替换模式进入紧凑查找，不遗留替换输入行。
#[test]
fn find_shortcut_keeps_replace_collapsed() {
    assert_eq!(SearchPanelMode::Closed.show_find(), SearchPanelMode::Find);
    assert_eq!(SearchPanelMode::Replace.show_find(), SearchPanelMode::Find);
    assert!(SearchPanelMode::Find.is_visible());
    assert!(!SearchPanelMode::Find.shows_replace());
}

/// Ctrl/Cmd+H 从任意面板状态进入替换模式，并由工具栏反向折叠。
#[test]
fn replace_shortcut_and_toolbar_toggle_share_modes() {
    assert_eq!(
        SearchPanelMode::Closed.show_replace(),
        SearchPanelMode::Replace
    );
    assert_eq!(
        SearchPanelMode::Find.show_replace(),
        SearchPanelMode::Replace
    );
    assert!(SearchPanelMode::Replace.shows_replace());
    assert_eq!(
        SearchPanelMode::Find.toggle_replace(),
        SearchPanelMode::Replace
    );
    assert_eq!(
        SearchPanelMode::Replace.toggle_replace(),
        SearchPanelMode::Find
    );
}

/// 关闭状态没有可见输入行，供 Escape 和显式关闭按钮共用同一业务状态。
#[test]
fn close_hides_both_find_and_replace() {
    let closed = SearchPanelMode::Replace.close();
    assert_eq!(closed, SearchPanelMode::Closed);
    assert!(!closed.is_visible());
    assert!(!closed.shows_replace());
}
