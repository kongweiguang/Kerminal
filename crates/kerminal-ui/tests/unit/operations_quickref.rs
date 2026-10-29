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

/// 固定速查项的中文说明和顺序与 main 工具栏相同，防止两套面板文案逐渐漂移。
///
/// @author kongweiguang
#[test]
fn quickref_labels_match_main_tool_panel_order() {
    assert_eq!(
        commands()
            .iter()
            .map(|item| item.label_zh)
            .collect::<Vec<_>>(),
        vec![
            "列出所有会话",
            "新建名为 work 的会话",
            "连接到 work 会话",
            "命令方式退出当前连接",
            "在 tmux 内切换会话",
            "新建名为 logs 的窗口",
            "左右分屏",
            "上下分屏",
            "查看 work 会话窗口",
            "重新加载 tmux 配置",
        ]
    );
    assert_eq!(
        shortcuts()
            .iter()
            .map(|item| item.label_zh)
            .collect::<Vec<_>>(),
        vec![
            "快捷键退出当前 tmux 连接",
            "新建窗口",
            "切到下一个窗口",
            "切到上一个窗口",
            "左右分屏",
            "上下分屏",
            "切换到下一个分屏",
            "关闭当前分屏，tmux 会确认",
            "进入复制模式",
            "查看 tmux 快捷键列表",
        ]
    );
}
