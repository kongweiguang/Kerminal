//! 快捷键持久模型与兼容默认动作目录。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

/// 快捷键行的 UI 投影；保存只修改同 action 的绑定字段，保留扩展元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeybindingPreference {
    pub action: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub binding: String,
    #[serde(default)]
    pub windows_binding: String,
    #[serde(default)]
    pub mac_binding: String,
    /// Linux 专用绑定；缺失时保留旧版非 macOS 的 Windows 字段回退语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linux_binding: Option<String>,
    /// Windows 附加绑定；None 表示旧配置未提供该字段，Some 空串表示显式禁用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_alternate_binding: Option<String>,
    pub scope: KeybindingScope,
    pub editable: bool,
}

/// 快捷键作用范围，值兼容旧版 keybindings 数组。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeybindingScope {
    Global,
    Terminal,
    Workspace,
}

/// 保留既有 action ID 和跨平台键位契约，标签切换名称覆盖工作区所有类型；用户自定义绑定仍由配置读取覆盖。
///
/// @author kongweiguang
pub(super) fn default_keybindings() -> Vec<KeybindingPreference> {
    let mut defaults = [
        (
            "settings.open",
            "打开设置",
            "Ctrl+Alt+S",
            "Ctrl+Alt+S",
            "Cmd+,",
            KeybindingScope::Global,
        ),
        (
            "settings.keybindings",
            "查看快捷键与动作",
            "Ctrl+Shift+A",
            "Ctrl+Shift+A",
            "Cmd+Shift+A",
            KeybindingScope::Global,
        ),
        (
            "terminal.focus",
            "回到终端",
            "Alt+F12",
            "Alt+F12",
            "Option+F12",
            KeybindingScope::Global,
        ),
        (
            "terminal.search",
            "搜索终端",
            "Ctrl+F",
            "Ctrl+F",
            "Cmd+F",
            KeybindingScope::Terminal,
        ),
        (
            "terminal.newTab",
            "新建本地终端",
            "Ctrl+Shift+T",
            "Ctrl+Shift+T",
            "Cmd+Shift+T",
            KeybindingScope::Workspace,
        ),
        (
            "terminal.closeTab",
            "关闭当前终端 tab",
            "Ctrl+F4",
            "Ctrl+F4",
            "Cmd+W",
            KeybindingScope::Workspace,
        ),
        (
            "terminal.closePane",
            "关闭当前分屏",
            "Ctrl+Shift+W",
            "Ctrl+Shift+W",
            "Cmd+Shift+W",
            KeybindingScope::Terminal,
        ),
        (
            "terminal.splitHorizontal",
            "左右分屏",
            "Ctrl+Alt+Right",
            "Ctrl+Alt+Right",
            "Ctrl+Option+Right",
            KeybindingScope::Workspace,
        ),
        (
            "terminal.splitVertical",
            "上下分屏",
            "Ctrl+Alt+Down",
            "Ctrl+Alt+Down",
            "Ctrl+Option+Down",
            KeybindingScope::Workspace,
        ),
        (
            "terminal.previousTab",
            "上一个标签",
            "Alt+Left",
            "Alt+Left",
            "Cmd+Shift+[",
            KeybindingScope::Workspace,
        ),
        (
            "terminal.nextTab",
            "下一个标签",
            "Alt+Right",
            "Alt+Right",
            "Cmd+Shift+]",
            KeybindingScope::Workspace,
        ),
        (
            "workspace.quickOpen",
            "快速打开",
            "Ctrl+P",
            "Ctrl+P",
            "Cmd+P",
            KeybindingScope::Workspace,
        ),
        (
            "workspace.commandPalette",
            "命令面板",
            "Ctrl+Shift+P",
            "Ctrl+Shift+P",
            "Cmd+Shift+P",
            KeybindingScope::Workspace,
        ),
        (
            "tool.agentLauncher",
            "打开 Agent Launcher",
            "Alt+2",
            "Alt+2",
            "Cmd+2",
            KeybindingScope::Global,
        ),
        (
            "tool.system",
            "打开系统信息",
            "Alt+3",
            "Alt+3",
            "Cmd+3",
            KeybindingScope::Global,
        ),
        (
            "tool.sftp",
            "打开 SFTP",
            "Alt+4",
            "Alt+4",
            "Cmd+4",
            KeybindingScope::Global,
        ),
        (
            "tool.ports",
            "打开端口转发",
            "Alt+5",
            "Alt+5",
            "Cmd+5",
            KeybindingScope::Global,
        ),
        (
            "tool.snippets",
            "打开脚本片段",
            "Alt+6",
            "Alt+6",
            "Cmd+6",
            KeybindingScope::Global,
        ),
        (
            "tool.logs",
            "打开日志",
            "Alt+7",
            "Alt+7",
            "Cmd+7",
            KeybindingScope::Global,
        ),
        (
            "application.quit",
            "退出 Kerminal",
            "Ctrl+Shift+Q",
            "Ctrl+Shift+Q",
            "Cmd+Q",
            KeybindingScope::Global,
        ),
    ]
    .into_iter()
    .map(
        |(action, label, binding, windows_binding, mac_binding, scope)| KeybindingPreference {
            action: action.to_owned(),
            label: label.to_owned(),
            description: String::new(),
            binding: binding.to_owned(),
            windows_binding: windows_binding.to_owned(),
            mac_binding: mac_binding.to_owned(),
            linux_binding: None,
            windows_alternate_binding: None,
            scope,
            editable: true,
        },
    )
    .collect::<Vec<_>>();
    defaults.push(default_terminal_paste_keybinding());
    defaults
}

/// 为系统剪贴板粘贴提供单一终端动作，并让平台默认值由各自绑定字段表达。
///
/// @author kongweiguang
pub(super) fn default_terminal_paste_keybinding() -> KeybindingPreference {
    KeybindingPreference {
        action: "terminal.paste".to_owned(),
        label: "粘贴到终端".to_owned(),
        description: String::new(),
        // 通用字段保持空值，确保平台字段显式清空时不会偷偷恢复另一平台的按键。
        binding: String::new(),
        windows_binding: "Ctrl+V".to_owned(),
        mac_binding: "Cmd+V".to_owned(),
        linux_binding: Some("Ctrl+Shift+V".to_owned()),
        windows_alternate_binding: Some("Ctrl+Shift+V".to_owned()),
        scope: KeybindingScope::Terminal,
        editable: true,
    }
}
