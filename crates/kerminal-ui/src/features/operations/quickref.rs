//! tmux 工具栏的安全速查条目；显示文本与发送给目标 pane 的 payload 分开保存。
//!
//! @author kongweiguang

use super::TmuxQuickrefKind;

/// 一条可复制或发送的固定参考内容，不接受用户提供的 shell 片段。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TmuxQuickrefItem {
    pub(super) label_zh: &'static str,
    pub(super) label_en: &'static str,
    pub(super) display: &'static str,
    pub(super) payload: &'static str,
    pub(super) kind: TmuxQuickrefKind,
}

const COMMANDS: [TmuxQuickrefItem; 10] = [
    TmuxQuickrefItem {
        label_zh: "列出所有会话",
        label_en: "List sessions",
        display: "tmux ls",
        payload: "tmux ls",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "新建 work 会话",
        label_en: "Create work session",
        display: "tmux new -s work",
        payload: "tmux new -s work",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "连接 work 会话",
        label_en: "Attach to work",
        display: "tmux attach -t work",
        payload: "tmux attach -t work",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "命令方式分离",
        label_en: "Detach client",
        display: "tmux detach-client",
        payload: "tmux detach-client",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "在 tmux 内切换会话",
        label_en: "Switch session",
        display: "tmux switch-client -t work",
        payload: "tmux switch-client -t work",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "新建 logs 窗口",
        label_en: "Create logs window",
        display: "tmux new-window -n logs",
        payload: "tmux new-window -n logs",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "左右分屏",
        label_en: "Split left/right",
        display: "tmux split-window -h",
        payload: "tmux split-window -h",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "上下分屏",
        label_en: "Split top/bottom",
        display: "tmux split-window -v",
        payload: "tmux split-window -v",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "查看 work 窗口",
        label_en: "List work windows",
        display: "tmux list-windows -t work",
        payload: "tmux list-windows -t work",
        kind: TmuxQuickrefKind::Command,
    },
    TmuxQuickrefItem {
        label_zh: "重新加载配置",
        label_en: "Reload tmux config",
        display: "tmux source-file ~/.tmux.conf",
        payload: "tmux source-file ~/.tmux.conf",
        kind: TmuxQuickrefKind::Command,
    },
];

const SHORTCUTS: [TmuxQuickrefItem; 10] = [
    TmuxQuickrefItem {
        label_zh: "退出当前连接",
        label_en: "Detach client",
        display: "Ctrl-b d",
        payload: "\u{2}d",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "新建窗口",
        label_en: "New window",
        display: "Ctrl-b c",
        payload: "\u{2}c",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "下一个窗口",
        label_en: "Next window",
        display: "Ctrl-b n",
        payload: "\u{2}n",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "上一个窗口",
        label_en: "Previous window",
        display: "Ctrl-b p",
        payload: "\u{2}p",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "左右分屏",
        label_en: "Split left/right",
        display: "Ctrl-b %",
        payload: "\u{2}%",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "上下分屏",
        label_en: "Split top/bottom",
        display: "Ctrl-b \"",
        payload: "\u{2}\"",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "切换分屏",
        label_en: "Next pane",
        display: "Ctrl-b o",
        payload: "\u{2}o",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "关闭当前分屏（会确认）",
        label_en: "Close pane (prompts)",
        display: "Ctrl-b x",
        payload: "\u{2}x",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "进入复制模式",
        label_en: "Copy mode",
        display: "Ctrl-b [",
        payload: "\u{2}[",
        kind: TmuxQuickrefKind::Shortcut,
    },
    TmuxQuickrefItem {
        label_zh: "查看快捷键",
        label_en: "List key bindings",
        display: "Ctrl-b ?",
        payload: "\u{2}?",
        kind: TmuxQuickrefKind::Shortcut,
    },
];

/// 返回旧版工具栏提供的常用命令，保持固定列表且不执行命令。
///
/// @author kongweiguang
pub(super) const fn commands() -> &'static [TmuxQuickrefItem] {
    &COMMANDS
}

/// 返回 tmux 默认 prefix 控制字节和按键组成的固定快捷键列表。
///
/// @author kongweiguang
pub(super) const fn shortcuts() -> &'static [TmuxQuickrefItem] {
    &SHORTCUTS
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_quickref.rs"]
mod tests;
