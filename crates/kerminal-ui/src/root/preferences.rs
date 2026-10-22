//! RootView 消费设置快照并把持久化偏好投影为工作台运行态。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window, WindowBackgroundAppearance};
use kerminal_application::preferences::{
    InterfaceDensity, InterfaceLanguage, SettingsSnapshot, TerminalColorScheme, ThemeMode,
};

use super::RootView;
use crate::{
    terminal_background::TerminalBackgroundSettings,
    terminal_interaction::TerminalInteractionPolicy,
    terminal_metrics::TerminalMetrics,
    theme::{BackgroundMaterialSettings, ThemeTokens, apply_theme_mode},
};

/// 从完整设置快照派生当前工作台使用的有限运行态字段，避免 UI 事件各自解释配置语义。
struct RuntimePreferences {
    theme_mode: ThemeMode,
    interface_language: InterfaceLanguage,
    terminal_color_scheme: TerminalColorScheme,
    terminal_metrics: TerminalMetrics,
    terminal_scrollback: usize,
    cursor_blink_enabled: bool,
}

/// 主工作区的间距和条高由一份密度投影驱动，Root 不自行解释持久化枚举。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct InterfaceDensityMetrics {
    pub(crate) item_gap: f32,
    pub(crate) sidebar_padding: f32,
    pub(crate) item_padding_x: f32,
    pub(crate) item_padding_y: f32,
    pub(crate) tab_bar_height: f32,
    pub(crate) status_bar_height: f32,
    pub(crate) tool_rail_padding_y: f32,
}

impl InterfaceDensityMetrics {
    /// 保持三个密度等级的间距和可见条高单调变化，避免只保存枚举却没有界面效果。
    pub(crate) fn from_density(density: InterfaceDensity) -> Self {
        match density {
            InterfaceDensity::Compact => Self {
                item_gap: 8.0,
                sidebar_padding: 12.0,
                item_padding_x: 8.0,
                item_padding_y: 5.0,
                tab_bar_height: 34.0,
                status_bar_height: 24.0,
                tool_rail_padding_y: 8.0,
            },
            InterfaceDensity::Comfortable => Self {
                item_gap: 12.0,
                sidebar_padding: 16.0,
                item_padding_x: 12.0,
                item_padding_y: 8.0,
                tab_bar_height: 38.0,
                status_bar_height: 26.0,
                tool_rail_padding_y: 12.0,
            },
            InterfaceDensity::Spacious => Self {
                item_gap: 16.0,
                sidebar_padding: 20.0,
                item_padding_x: 16.0,
                item_padding_y: 10.0,
                tab_bar_height: 46.0,
                status_bar_height: 32.0,
                tool_rail_padding_y: 16.0,
            },
        }
    }
}

impl RuntimePreferences {
    /// 只投影已有 Root 消费者的字段，并用窗口文本系统的安装清单解析终端字体。
    ///
    /// @author kongweiguang
    fn from_snapshot(snapshot: &SettingsSnapshot, installed_font_families: &[String]) -> Self {
        Self {
            theme_mode: snapshot.theme_mode,
            interface_language: snapshot.interface_language,
            terminal_color_scheme: snapshot.terminal.color_scheme,
            terminal_metrics: TerminalMetrics::from_settings(
                &snapshot.terminal,
                installed_font_families,
            ),
            terminal_scrollback: usize::try_from(snapshot.terminal.scrollback).unwrap_or(5_000),
            cursor_blink_enabled: snapshot.terminal.cursor_blink,
        }
    }
}

/// 同一快照统一驱动主题、侧栏密度、locale 和现有终端，保存后的展示不依赖重新打开窗口。
///
/// @author kongweiguang
pub(super) fn apply_snapshot(
    root: &mut RootView,
    snapshot: SettingsSnapshot,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let installed_font_families = window.text_system().all_font_names();
    let runtime = RuntimePreferences::from_snapshot(&snapshot, &installed_font_families);
    root.settings_snapshot = Some(snapshot.clone());
    root.theme_mode = runtime.theme_mode;
    root.terminal_color_scheme = runtime.terminal_color_scheme;
    root.terminal_metrics = runtime.terminal_metrics;
    root.terminal_scrollback = runtime.terminal_scrollback;
    root.cursor_blink_enabled = runtime.cursor_blink_enabled;
    if let Some(navigation) = root.host_navigation.as_ref() {
        navigation.update(cx, |navigation, cx| {
            navigation.set_interface_density(snapshot.interface_density, cx);
        });
    }

    apply_theme_mode(root.theme_mode, root.window_appearance, cx);
    cx.set_global(BackgroundMaterialSettings::from_snapshot(&snapshot));
    apply_window_background_appearance(window, ThemeTokens::current(window, cx).materials);
    root.window_appearance = window.appearance();
    let previous_locale = crate::locale::current_language();
    let applied_locale = crate::locale::apply_interface_language(
        runtime.interface_language,
        root.services.system_locale.as_deref().unwrap_or("en-US"),
    );
    if previous_locale != applied_locale {
        refresh_cached_locales(root, window, cx);
    }
    refresh_keyword_rules(root, window, cx);
    root.resize_active_terminal(window, cx);
    root.queue_terminal_palette_update(cx);
    cx.refresh_windows();
    cx.notify();
}

/// 只刷新 Root 已持有实体中的本地化缓存，不创建懒加载面板或覆盖用户输入状态。
///
/// @author kongweiguang
fn refresh_cached_locales(root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>) {
    root.preferences_panel
        .update(cx, |panel, cx| panel.refresh_locale(window, cx));

    for files in root.files_pane.iter().chain(root.sftp_panel.iter()) {
        files.update(cx, |panel, cx| panel.refresh_locale(window, cx));
    }
    for files in root.workspace_files.files_panes() {
        files.update(cx, |panel, cx| panel.refresh_locale(window, cx));
    }
    if let Some(editor) = root.file_editor_workspace.clone() {
        editor.update(cx, |workspace, cx| workspace.refresh_locale(window, cx));
    }
    if let Some(commands) = root.commands_view.clone() {
        commands.update(cx, |view, cx| view.refresh_locale(window, cx));
    }
    if let Some(logs) = root.logs_view.clone() {
        logs.update(cx, |view, cx| view.refresh_locale(window, cx));
    }
    if let Some(connections) = root.connections_panel.clone() {
        connections.update(cx, |panel, cx| panel.refresh_locale(window, cx));
    }
    if let Some(navigation) = root.host_navigation.clone() {
        navigation.update(cx, |panel, cx| panel.refresh_locale(window, cx));
    }
    if let Some(agents) = root.agents_panel.clone() {
        agents.update(cx, |panel, cx| panel.refresh_locale(window, cx));
    }

    let operations = root
        .tool_rail_operations_views
        .values()
        .cloned()
        .chain(root.operations_view.clone())
        .collect::<Vec<_>>();
    for view in operations {
        view.update(cx, |panel, cx| panel.refresh_locale(window, cx));
    }

    let terminal_tools = root
        .workspace_tools
        .values()
        .cloned()
        .chain(root.agent_terminal_tools.clone())
        .collect::<Vec<_>>();
    for view in terminal_tools {
        view.update(cx, |panel, cx| panel.refresh_locale(window, cx));
    }
}

/// 仅当主题材料自身保证全视口不透明时使用原生不透明表面，图片状态不参与此证明。
///
/// @author kongweiguang
fn apply_window_background_appearance(
    window: &mut Window,
    materials: crate::theme::BackgroundMaterials,
) {
    window.set_background_appearance(if materials.compositing_is_opaque {
        WindowBackgroundAppearance::Opaque
    } else {
        WindowBackgroundAppearance::Transparent
    });
}

/// 让 Root 在布局时读取同一份密度指标；设置尚未加载时采用明确的紧凑缺省值。
pub(crate) fn interface_density_metrics(root: &RootView) -> InterfaceDensityMetrics {
    interface_density_metrics_from_snapshot(root.settings_snapshot.as_ref())
}

/// 把持久化密度只映射到实际 spacing/height，不让页面或 panel 再维护另一套阈值。
fn interface_density_metrics_from_snapshot(
    snapshot: Option<&SettingsSnapshot>,
) -> InterfaceDensityMetrics {
    InterfaceDensityMetrics::from_density(
        snapshot
            .map(|settings| settings.interface_density)
            .unwrap_or_default(),
    )
}

/// 从 Root 保存的快照生成背景绘制设置；未完成首次读取时不猜测用户配置。
pub(super) fn background_settings(root: &RootView) -> Option<TerminalBackgroundSettings> {
    background_settings_from_snapshot(root.settings_snapshot.as_ref())
}

/// 从 Root 保存的快照生成按键与鼠标策略；加载前交由工作台既有默认行为处理。
pub(super) fn interaction_policy(root: &RootView) -> Option<TerminalInteractionPolicy> {
    interaction_policy_from_snapshot(root.settings_snapshot.as_ref())
}

/// 关键词只从已加载快照刷新各 pane；失败提示和旧规则保留由工具叶子统一负责。
///
/// @author kongweiguang
pub(super) fn refresh_keyword_rules(
    root: &mut RootView,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let Some(rules) = root
        .settings_snapshot
        .as_ref()
        .map(|snapshot| snapshot.terminal.keyword_highlights.clone())
    else {
        return;
    };
    let theme_mode = root.theme_mode;
    let appearance = window.appearance();
    let tools = root
        .workspace_tools
        .values()
        .cloned()
        .chain(root.agent_terminal_tools.clone())
        .collect::<Vec<_>>();
    for tools in tools {
        tools.update(cx, |tools, cx| {
            // The view presents a deduplicated safe error and retains its last valid matcher.
            let _ = tools.update_keyword_rules(rules.clone(), theme_mode, appearance, cx);
        });
    }
}

/// 纯设置投影便于测试加载前的空态与图片参数一致性。
fn background_settings_from_snapshot(
    snapshot: Option<&SettingsSnapshot>,
) -> Option<TerminalBackgroundSettings> {
    snapshot.map(TerminalBackgroundSettings::from_snapshot)
}

/// 纯设置投影让 Root 与终端输入处理共用唯一交互策略映射。
fn interaction_policy_from_snapshot(
    snapshot: Option<&SettingsSnapshot>,
) -> Option<TerminalInteractionPolicy> {
    snapshot.map(TerminalInteractionPolicy::from_snapshot)
}

#[cfg(test)]
#[path = "../../tests/unit/root_preferences.rs"]
mod tests;
