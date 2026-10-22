//! 固定 tmux 速查内容保持安全 payload 与旧工具栏覆盖范围。
//!
//! @author kongweiguang

use super::{commands, shortcuts};
use crate::features::operations::TmuxQuickrefKind;

/// 命令速查覆盖常用旧版入口，且复制内容是可读命令文本而非任意脚本输入。
///
/// @author kongweiguang
#[test]
fn quickref_commands_are_fixed_and_readable() {
    let items = commands();

    assert_eq!(items.len(), 10);
    assert_eq!(items.len() + shortcuts().len(), 20);
    assert!(
        items
            .iter()
            .all(|item| item.kind == TmuxQuickrefKind::Command)
    );
    assert!(items.iter().all(|item| !item.payload.is_empty()));
    assert!(
        items
            .iter()
            .all(|item| !item.payload.chars().any(char::is_control))
    );
}

/// tmux 快捷键发送 payload 保留 prefix 控制字节，展示标签仍是人类可读按键。
#[test]
fn quickref_shortcuts_preserve_tmux_prefix_bytes() {
    let items = shortcuts();

    assert_eq!(items.len(), 10);
    assert!(
        items
            .iter()
            .all(|item| item.kind == TmuxQuickrefKind::Shortcut)
    );
    assert!(items.iter().all(|item| item.payload.starts_with('\u{2}')));
    assert!(
        items
            .iter()
            .all(|item| !item.display.chars().any(char::is_control))
    );
}
