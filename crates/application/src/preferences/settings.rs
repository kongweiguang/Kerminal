//! 设置应用投影、字段定义与领域校验；磁盘编解码由 Infrastructure 负责。
//!
//! @author kongweiguang

use kerminal_domain::preferences::ConfigRevision;
use serde::{Deserialize, Serialize};

use crate::{AppError, AppResult};

mod advanced;
mod keybindings;
mod terminal;

use self::keybindings::default_keybindings;
pub use advanced::{
    AgentLauncherSettings, CustomAgentDefinition, DesktopNotificationSettings,
    ExternalLaunchSettings, ExternalLaunchToolSetting, SftpPerformanceSettings,
    ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId, WorkbenchLayoutSettings,
};
pub use keybindings::{KeybindingPreference, KeybindingScope};
pub use terminal::{
    TerminalColorScheme, TerminalCommandSuggestionMenuShortcut,
    TerminalCommandSuggestionPresentation, TerminalCommandSuggestionRemoteRefresh,
    TerminalCursorStyle, TerminalFontWeight, TerminalInlineSuggestionAcceptKey,
    TerminalInlineSuggestionProviderSettings, TerminalInlineSuggestionSettings,
    TerminalKeywordHighlightColorPair, TerminalKeywordHighlightCustomColors,
    TerminalKeywordHighlightMatchMode, TerminalKeywordHighlightRule,
    TerminalKeywordHighlightSettings, TerminalKeywordHighlightStyle, TerminalRightClickBehavior,
    TerminalSettings,
};
/// 设置用例使用的稳定应用投影；它不直接描述任何 TOML 表层级。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub theme_mode: ThemeMode,
    #[serde(default)]
    pub app_color_scheme: AppColorScheme,
    pub interface_language: InterfaceLanguage,
    pub interface_density: InterfaceDensity,
    pub background_enabled: bool,
    pub background_fit: BackgroundFit,
    pub background_image_path: String,
    pub background_opacity: u8,
    pub window_opacity: u8,
    pub terminal: TerminalSettings,
    pub keybindings: Vec<KeybindingPreference>,
    pub desktop_notifications: DesktopNotificationSettings,
    pub external_launch: ExternalLaunchSettings,
    pub agent_launcher: AgentLauncherSettings,
    pub sftp: SftpPerformanceSettings,
    pub tool_rail: ToolRailSettings,
    pub workbench_layout: WorkbenchLayoutSettings,
}

impl Default for SettingsSnapshot {
    /// 提供新安装和缺失设置文档的产品缺省值，不在首次启动时写出文件。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::Dark,
            app_color_scheme: AppColorScheme::default(),
            interface_language: InterfaceLanguage::System,
            interface_density: InterfaceDensity::Compact,
            background_enabled: false,
            background_fit: BackgroundFit::Cover,
            background_image_path: String::new(),
            background_opacity: 100,
            window_opacity: 100,
            terminal: TerminalSettings::default(),
            keybindings: default_keybindings(),
            desktop_notifications: DesktopNotificationSettings::default(),
            external_launch: ExternalLaunchSettings::default(),
            agent_launcher: AgentLauncherSettings::default(),
            sftp: SftpPerformanceSettings::default(),
            tool_rail: ToolRailSettings::default(),
            workbench_layout: WorkbenchLayoutSettings::default(),
        }
    }
}

impl SettingsSnapshot {
    /// 校验可编辑范围并保留用户选定的动作集合，避免日常读取重新启用已移除动作。
    ///
    /// @author kongweiguang
    pub fn validated(mut self) -> AppResult<Self> {
        self.background_image_path = self.background_image_path.trim().to_owned();
        if self.background_image_path.len() > 1024 {
            return Err(AppError::invalid("背景图路径不能超过 1024 个字符"));
        }
        self.background_opacity = self.background_opacity.min(100);
        self.window_opacity = self.window_opacity.clamp(35, 100);
        self.terminal.font_family = self.terminal.font_family.trim().to_owned();
        if self.terminal.font_family.is_empty() || self.terminal.font_family.len() > 160 {
            return Err(AppError::invalid("终端字体配置无效"));
        }
        if !(10..=24).contains(&self.terminal.font_size) {
            return Err(AppError::invalid("终端字号需要在 10 到 24 之间"));
        }
        if !(1.0..=1.8).contains(&self.terminal.line_height) {
            return Err(AppError::invalid("终端行高需要在 1.0 到 1.8 之间"));
        }
        if !(1_000..=50_000).contains(&self.terminal.scrollback) {
            return Err(AppError::invalid("滚屏缓冲需要在 1000 到 50000 行之间"));
        }
        self.terminal = self.terminal.validated()?;
        self.desktop_notifications = self.desktop_notifications.normalized();
        self.external_launch = self.external_launch.normalized();
        self.agent_launcher = self.agent_launcher.validated()?;
        self.sftp = self.sftp.normalized();
        self.tool_rail = self.tool_rail.normalized();
        self.workbench_layout = self.workbench_layout.normalized();
        let mut actions = std::collections::BTreeSet::new();
        if self.keybindings.iter().any(|binding| {
            binding.action.trim().is_empty()
                || binding.binding.len() > 128
                || binding.windows_binding.len() > 128
                || binding.mac_binding.len() > 128
                || binding
                    .linux_binding
                    .as_ref()
                    .is_some_and(|value| value.len() > 128)
                || binding
                    .windows_alternate_binding
                    .as_ref()
                    .is_some_and(|value| value.len() > 128)
                || !actions.insert(binding.action.clone())
        }) {
            return Err(AppError::invalid("快捷键配置重复或超出长度限制"));
        }
        Ok(self)
    }
}

/// 深浅色主题策略；值沿用磁盘 TOML 的现行枚举拼写。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    System,
}

/// 应用界面的内置色板；终端 ANSI 与光标颜色由独立设置控制。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppColorScheme {
    #[default]
    Kerminal,
    #[serde(rename = "jetbrains")]
    JetBrains,
    Fleet,
    Xcode,
    Claude,
    #[serde(rename = "vscode")]
    VsCode,
}

impl AppColorScheme {
    /// 内置选项顺序作为设置界面的稳定顺序，避免显示与持久化各自维护列表。
    pub const ALL: [Self; 6] = [
        Self::Kerminal,
        Self::JetBrains,
        Self::Fleet,
        Self::Xcode,
        Self::Claude,
        Self::VsCode,
    ];

    /// 返回与 TOML 序列化一致的稳定标识，避免用本地化标签作为配置值。
    ///
    /// @author kongweiguang
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kerminal => "kerminal",
            Self::JetBrains => "jetbrains",
            Self::Fleet => "fleet",
            Self::Xcode => "xcode",
            Self::Claude => "claude",
            Self::VsCode => "vscode",
        }
    }
}

/// 设置界面语言策略；System 表示沿用当前应用语言选择。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterfaceLanguage {
    #[default]
    System,
    ZhCn,
    EnUs,
}

/// 工作台界面密度，与旧版 TOML 枚举保持一致。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterfaceDensity {
    #[default]
    Compact,
    Comfortable,
    Spacious,
}

/// 背景图铺放方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundFit {
    #[default]
    Cover,
    Contain,
    Tile,
}

/// 一次加载结果中的 UI 设置与原始文件 revision；缺失文件以 None 表示尚未持久化。
#[derive(Debug, Clone, PartialEq)]
pub struct VersionedSettings {
    pub settings: SettingsSnapshot,
    pub revision: Option<ConfigRevision>,
}
