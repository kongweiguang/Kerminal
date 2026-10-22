//! 将偏好设置投影成终端键鼠与显示行为，不让事件处理器重复解释磁盘设置。
//!
//! @author kongweiguang

use kerminal_application::preferences::{SettingsSnapshot, TerminalRightClickBehavior};

/// 纯粹的终端交互策略快照；视图切换不会改变它，设置保存后由 Root 替换整份投影。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalInteractionPolicy {
    mac_option_is_meta: bool,
    right_click_behavior: TerminalRightClickBehavior,
    selection_copy: bool,
    show_command_block_rail: bool,
    show_tab_numbers: bool,
}

/// 右键事件的动作决策；FocusOnly 仍由事件调用方负责把焦点交还终端。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalRightClickAction {
    FocusOnly,
    Paste,
    Menu,
}

impl TerminalInteractionPolicy {
    /// 从成功加载的设置快照提取终端事件策略，保留字段原意而不读取配置或平台状态。
    pub(crate) fn from_snapshot(settings: &SettingsSnapshot) -> Self {
        Self {
            mac_option_is_meta: settings.terminal.mac_option_is_meta,
            right_click_behavior: settings.terminal.right_click_behavior,
            selection_copy: settings.terminal.selection_copy,
            show_command_block_rail: settings.terminal.show_command_block_rail,
            show_tab_numbers: settings.terminal.show_tab_numbers,
        }
    }

    /// 仅 macOS 输入边界使用旧配置原值；其它平台不编译无调用 getter，测试仍可核对原值投影。
    ///
    /// @author kongweiguang
    #[cfg(any(target_os = "macos", test))]
    pub(crate) const fn mac_option_is_meta(self) -> bool {
        self.mac_option_is_meta
    }

    /// 返回选择变化后是否自动复制非空文本；空选择处理由剪贴板调用方负责。
    pub(crate) const fn copy_on_selection(self) -> bool {
        self.selection_copy
    }

    /// 命令块轨道还受 shell integration 能力门控，避免设置强行显示空轨道。
    pub(crate) const fn command_block_rail_visible(self, shell_assist_enabled: bool) -> bool {
        self.show_command_block_rail && shell_assist_enabled
    }
}

#[cfg(test)]
#[path = "../tests/unit/terminal_interaction.rs"]
mod tests;
