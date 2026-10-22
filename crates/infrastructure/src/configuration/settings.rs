//! settings.toml 的磁盘 DTO、解码与保留式 patch。
//!
//! @author kongweiguang

use kerminal_application::preferences::{
    AgentLauncherSettings, BackgroundFit, DesktopNotificationSettings, ExternalLaunchSettings,
    InterfaceDensity, InterfaceLanguage, KeybindingPreference, SettingsSnapshot,
    SftpPerformanceSettings, TerminalSettings, ThemeMode, ToolRailSettings,
    WorkbenchLayoutSettings,
};
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use kerminal_application::{AppError, AppResult};

mod patch;

/// 磁盘 DTO 固定旧版 appearance 嵌套结构，避免应用投影改变文件 schema。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct SettingsDocument {
    theme_mode: ThemeMode,
    interface_density: InterfaceDensity,
    appearance: AppearanceSettings,
    terminal: TerminalSettings,
    keybindings: Vec<KeybindingPreference>,
    desktop_notifications: DesktopNotificationSettings,
    external_launch: ExternalLaunchSettings,
    agent_launcher: AgentLauncherSettings,
    sftp: SftpPerformanceSettings,
    tool_rail: ToolRailSettings,
    workbench_layout: WorkbenchLayoutSettings,
}

/// appearance 表承载语言、背景与透明度设置，字段名沿用旧 TOML 契约。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct AppearanceSettings {
    interface_language: InterfaceLanguage,
    background_enabled: bool,
    background_fit: BackgroundFit,
    background_image_path: String,
    background_opacity: u8,
    window_opacity: u8,
}

impl Default for SettingsDocument {
    /// 缺省磁盘字段映射自应用默认值，首次读取不因此创建或改写文件。
    fn default() -> Self {
        Self::from_snapshot(SettingsSnapshot::default())
    }
}

impl SettingsDocument {
    /// 将应用投影转换为兼容磁盘 DTO，显式把扁平 appearance 字段放回子表。
    ///
    /// @author kongweiguang
    fn from_snapshot(settings: SettingsSnapshot) -> Self {
        Self {
            theme_mode: settings.theme_mode,
            interface_density: settings.interface_density,
            appearance: AppearanceSettings {
                interface_language: settings.interface_language,
                background_enabled: settings.background_enabled,
                background_fit: settings.background_fit,
                background_image_path: settings.background_image_path,
                background_opacity: settings.background_opacity,
                window_opacity: settings.window_opacity,
            },
            terminal: settings.terminal,
            keybindings: settings.keybindings,
            desktop_notifications: settings.desktop_notifications,
            external_launch: settings.external_launch,
            agent_launcher: settings.agent_launcher,
            sftp: settings.sftp,
            tool_rail: settings.tool_rail,
            workbench_layout: settings.workbench_layout,
        }
    }

    /// 将磁盘层级转换成 application 快照，不把文件表结构传给用例或 UI。
    ///
    /// @author kongweiguang
    fn into_snapshot(self) -> SettingsSnapshot {
        SettingsSnapshot {
            theme_mode: self.theme_mode,
            interface_language: self.appearance.interface_language,
            interface_density: self.interface_density,
            background_enabled: self.appearance.background_enabled,
            background_fit: self.appearance.background_fit,
            background_image_path: self.appearance.background_image_path,
            background_opacity: self.appearance.background_opacity,
            window_opacity: self.appearance.window_opacity,
            terminal: self.terminal,
            keybindings: self.keybindings,
            desktop_notifications: self.desktop_notifications,
            external_launch: self.external_launch,
            agent_launcher: self.agent_launcher,
            sftp: self.sftp,
            tool_rail: self.tool_rail,
            workbench_layout: self.workbench_layout,
        }
    }
}

/// 在基础设施边界读取旧文件结构；未知键由 TOML 解析器忽略并由原文仓储保留。
pub(super) fn decode_settings(source: &str) -> Result<SettingsSnapshot, toml::de::Error> {
    toml::from_str::<SettingsDocument>(source).map(SettingsDocument::into_snapshot)
}

/// 对原始 TOML 做注释和扩展字段保留式更新，仅在创建文件时初始化 schema 版本。
pub(super) fn patch_settings_document(
    source: Option<&str>,
    settings: &SettingsSnapshot,
) -> AppResult<String> {
    let mut document = match source {
        Some(source) => source
            .parse::<DocumentMut>()
            .map_err(|error| AppError::configuration_corrupt("设置").with_source(error))?,
        None => DocumentMut::new(),
    };
    if source.is_none() {
        document["schema_version"] = toml_edit::value(1);
    }
    patch::apply_settings_snapshot(&mut document, settings)?;
    Ok(document.to_string())
}

/// 只把经过领域校验的投影写成嵌套磁盘表，避免 TOML patch 依赖应用 DTO 的 serde 形状。
fn serialize_settings_document(settings: &SettingsSnapshot) -> AppResult<DocumentMut> {
    toml_edit::ser::to_document(&SettingsDocument::from_snapshot(settings.clone()))
        .map_err(|error| AppError::configuration_corrupt("设置").with_source(error))
}
