//! Root 设置快照投影的纯逻辑回归测试。
//!
//! @author kongweiguang

use kerminal_application::preferences::{
    BackgroundFit, InterfaceDensity, InterfaceLanguage, SettingsSnapshot, TerminalColorScheme,
    TerminalCursorStyle, TerminalFontWeight, TerminalRightClickBehavior, ThemeMode,
};

use crate::{
    root::preferences::{
        InterfaceDensityMetrics, RuntimePreferences, background_settings_from_snapshot,
        interaction_policy_from_snapshot, interface_density_metrics_from_snapshot,
    },
    terminal_background::TerminalBackgroundSettings,
    terminal_interaction::TerminalInteractionPolicy,
    theme::BackgroundMaterialSettings,
};

/// 分别选择的浅深色盘必须被生产运行态消费，跟随系统切换也不能退回旧通用值。
/// @author kongweiguang
#[test]
fn terminal_palette_selection_tracks_theme_and_system_appearance() {
    use crate::root::preferences::terminal_scheme_for_snapshot;
    use gpui_kit::WindowAppearance;
    let mut snapshot = SettingsSnapshot::default();
    snapshot.terminal.light_color_scheme = TerminalColorScheme::Github;
    snapshot.terminal.dark_color_scheme = TerminalColorScheme::TokyoNight;
    for (mode, appearance, expected) in [
        (
            ThemeMode::Light,
            WindowAppearance::Dark,
            TerminalColorScheme::Github,
        ),
        (
            ThemeMode::Dark,
            WindowAppearance::Light,
            TerminalColorScheme::TokyoNight,
        ),
        (
            ThemeMode::System,
            WindowAppearance::Light,
            TerminalColorScheme::Github,
        ),
        (
            ThemeMode::System,
            WindowAppearance::Dark,
            TerminalColorScheme::TokyoNight,
        ),
    ] {
        snapshot.theme_mode = mode;
        assert_eq!(
            terminal_scheme_for_snapshot(&snapshot, appearance),
            expected
        );
    }
}

/// 用明确可用的 GPUI 字体清单验证快照会跳过未安装的首选字体，再统一投影其它运行态字段。
///
/// @author kongweiguang
#[test]
fn runtime_preferences_project_theme_locale_and_terminal_values() {
    let mut snapshot = SettingsSnapshot {
        theme_mode: ThemeMode::Light,
        interface_language: InterfaceLanguage::EnUs,
        ..SettingsSnapshot::default()
    };
    snapshot.terminal.color_scheme = TerminalColorScheme::Solarized;
    snapshot.terminal.font_family = "JetBrains Mono, Consolas".to_owned();
    snapshot.terminal.font_size = 18;
    snapshot.terminal.font_weight = TerminalFontWeight::Medium;
    snapshot.terminal.line_height = 1.4;
    snapshot.terminal.cursor_style = TerminalCursorStyle::Bar;
    snapshot.terminal.cursor_blink = false;
    snapshot.terminal.scrollback = 12_345;

    let installed_font_families = ["Consolas"].map(str::to_owned);
    let runtime = RuntimePreferences::from_snapshot(&snapshot, &installed_font_families);

    assert_eq!(runtime.theme_mode, ThemeMode::Light);
    assert_eq!(runtime.interface_language, InterfaceLanguage::EnUs);
    assert_eq!(
        runtime.terminal_color_scheme,
        TerminalColorScheme::Solarized
    );
    assert_eq!(runtime.terminal_scrollback, 12_345);
    assert!(!runtime.cursor_blink_enabled);
    assert_eq!(runtime.terminal_metrics.font_family.to_string(), "Consolas");
    assert!(
        installed_font_families
            .iter()
            .any(|family| family.as_str() == runtime.terminal_metrics.font_family.as_ref())
    );
    assert_eq!(runtime.terminal_metrics.font_size, 18.0);
    assert!((runtime.terminal_metrics.line_height - 25.2).abs() < 0.001);
    assert_eq!(
        runtime.terminal_metrics.font_weight,
        TerminalFontWeight::Medium
    );
    assert_eq!(
        runtime.terminal_metrics.cursor_style,
        TerminalCursorStyle::Bar
    );
}

/// 首次配置读取前不伪造背景或键鼠策略，避免临时缺失值覆盖工作台缺省处理。
#[test]
fn derived_settings_are_absent_until_a_snapshot_is_loaded() {
    assert!(background_settings_from_snapshot(None).is_none());
    assert!(interaction_policy_from_snapshot(None).is_none());
}

/// 背景绘制和事件策略复用同一设置快照；右键与标签序号由实际 Canvas、Tab 入口测试，不保留死 getter 镜像。
///
/// @author kongweiguang
#[test]
fn background_and_interaction_getters_share_snapshot_projection() {
    let mut snapshot = SettingsSnapshot {
        background_enabled: true,
        background_fit: BackgroundFit::Tile,
        background_image_path: "assets/terminal.png".to_owned(),
        background_opacity: 73,
        ..SettingsSnapshot::default()
    };
    snapshot.terminal.mac_option_is_meta = true;
    snapshot.terminal.right_click_behavior = TerminalRightClickBehavior::Paste;
    snapshot.terminal.selection_copy = true;
    snapshot.terminal.show_command_block_rail = false;
    snapshot.terminal.show_tab_numbers = true;

    let background = background_settings_from_snapshot(Some(&snapshot));
    let policy = interaction_policy_from_snapshot(Some(&snapshot));

    assert_eq!(
        background,
        Some(TerminalBackgroundSettings::from_snapshot(&snapshot))
    );
    assert_eq!(
        policy,
        Some(TerminalInteractionPolicy::from_snapshot(&snapshot))
    );
    let policy = policy.expect("snapshot must produce an interaction policy");
    assert!(policy.mac_option_is_meta());
    assert!(policy.copy_on_selection());
    assert!(!policy.command_block_rail_visible(true));
}

/// 密度变化必须改变工作区间距和条高，缺省值则保持为紧凑布局。
///
/// @author kongweiguang
#[test]
fn interface_density_projects_real_spacing_and_height_metrics() {
    let compact = interface_density_metrics_from_snapshot(None);
    let comfortable_settings = SettingsSnapshot {
        interface_density: InterfaceDensity::Comfortable,
        ..SettingsSnapshot::default()
    };
    let comfortable = interface_density_metrics_from_snapshot(Some(&comfortable_settings));
    let spacious_settings = SettingsSnapshot {
        interface_density: InterfaceDensity::Spacious,
        ..SettingsSnapshot::default()
    };
    let spacious = interface_density_metrics_from_snapshot(Some(&spacious_settings));

    assert_eq!(
        compact,
        InterfaceDensityMetrics::from_density(InterfaceDensity::Compact)
    );
    assert!(compact.sidebar_padding < comfortable.sidebar_padding);
    assert!(comfortable.sidebar_padding < spacious.sidebar_padding);
    assert!(compact.item_gap < comfortable.item_gap);
    assert!(comfortable.item_gap < spacious.item_gap);
    assert!(compact.item_padding_x < comfortable.item_padding_x);
    assert!(comfortable.item_padding_x < spacious.item_padding_x);
    assert!(compact.item_padding_y < comfortable.item_padding_y);
    assert!(comfortable.item_padding_y < spacious.item_padding_y);
    assert!(compact.tab_bar_height < comfortable.tab_bar_height);
    assert!(comfortable.tab_bar_height < spacious.tab_bar_height);
    assert!(compact.status_bar_height < comfortable.status_bar_height);
    assert!(comfortable.status_bar_height < spacious.status_bar_height);
    assert!(compact.tool_rail_padding_y < comfortable.tool_rail_padding_y);
    assert!(comfortable.tool_rail_padding_y < spacious.tool_rail_padding_y);
}

/// 原始背景系数归一化后可稳定复投影，避免保留旧窗口色 alpha 算法的第二份状态。
/// @author kongweiguang
#[test]
fn background_material_settings_normalize_and_remain_idempotent() {
    let mut snapshot = SettingsSnapshot {
        window_opacity: 60,
        background_enabled: true,
        background_opacity: 45,
        background_image_path: "C:/wallpaper.png".to_owned(),
        ..SettingsSnapshot::default()
    };

    let projected = BackgroundMaterialSettings::from_snapshot(&snapshot);
    assert_ne!(projected, BackgroundMaterialSettings::default());
    assert_eq!(
        projected,
        BackgroundMaterialSettings::from_snapshot(&snapshot)
    );

    snapshot.window_opacity = 101;
    let over_limit = BackgroundMaterialSettings::from_snapshot(&snapshot);
    snapshot.window_opacity = 100;
    assert_eq!(
        over_limit,
        BackgroundMaterialSettings::from_snapshot(&snapshot)
    );

    snapshot.window_opacity = 1;
    let under_limit = BackgroundMaterialSettings::from_snapshot(&snapshot);
    snapshot.window_opacity = 35;
    assert_eq!(
        under_limit,
        BackgroundMaterialSettings::from_snapshot(&snapshot)
    );
}
