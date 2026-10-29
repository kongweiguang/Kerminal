//! GPUI 终端设置页：保留 main 的主题、字体预览、交互与光标选项。
//!
//! @author kongweiguang

use gpui_kit::{
    Animation, AnimationExt as _, ClickEvent, Context, Entity, IntoElement, SharedString, Window,
    assets::IconName,
    component::{
        Disableable as _, FocusableExt as _, Selectable as _, Sizable as _,
        button::{Button, ButtonCustomVariant, ButtonVariants as _},
        input::{InputState, NumberStep},
        menu::{DropdownMenu as _, PopupMenuItem},
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::{
    SettingsSnapshot, TerminalColorScheme, TerminalCursorStyle, TerminalFontWeight,
    TerminalRightClickBehavior, ThemeMode,
};
use std::time::Duration;

use crate::{terminal_metrics::TerminalMetrics, theme::ThemeTokens};

use super::panel::{PreferencesPanel, SettingsInputs};

#[derive(Clone, Copy)]
struct SchemeOption {
    id: &'static str,
    value: TerminalColorScheme,
    label: &'static str,
    description: &'static str,
    swatches: [u32; 4],
}

/// 下拉控件的字段名、当前值和可选项成组传递，避免调用点遗漏可见标签。
///
/// @author kongweiguang
struct SettingsDropdownSpec<T> {
    id: &'static str,
    label: SharedString,
    accessibility_label: SharedString,
    current_label: SharedString,
    current: T,
    options: Vec<(T, SharedString)>,
    update: fn(&mut SettingsSnapshot, T),
}

const SCHEME_OPTIONS: [SchemeOption; 4] = [
    SchemeOption {
        id: "kerminal",
        value: TerminalColorScheme::Kerminal,
        label: "Kerminal",
        description: "默认终端主题。",
        swatches: [0x1f1f21, 0x60a5fa, 0x4ade80, 0xfacc15],
    },
    SchemeOption {
        id: "tokyo-night",
        value: TerminalColorScheme::TokyoNight,
        label: "Tokyo Night",
        description: "低亮度夜间主题。",
        swatches: [0x1a1b26, 0x7aa2f7, 0x9ece6a, 0xbb9af7],
    },
    SchemeOption {
        id: "solarized",
        value: TerminalColorScheme::Solarized,
        label: "Solarized",
        description: "经典低对比。",
        swatches: [0x002b36, 0x268bd2, 0x859900, 0xb58900],
    },
    SchemeOption {
        id: "github",
        value: TerminalColorScheme::Github,
        label: "GitHub",
        description: "高对比代码色盘。",
        swatches: [0xffffff, 0x0969da, 0x1a7f37, 0x9a6700],
    },
];

#[derive(Clone, Copy)]
pub(in crate::features::preferences) struct TerminalPreviewColors {
    pub(in crate::features::preferences) background: u32,
    pub(in crate::features::preferences) foreground: u32,
    pub(in crate::features::preferences) green: u32,
    pub(in crate::features::preferences) cyan: u32,
    cursor: u32,
}

const FONT_OPTIONS: [(&str, &str); 15] = [
    (
        "JetBrains Mono",
        "\"JetBrains Mono\", \"JetBrains Mono NL\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "JetBrainsMono Nerd Font",
        "\"JetBrainsMono Nerd Font\", \"JetBrainsMonoNL Nerd Font\", \"JetBrains Mono\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "Fira Code",
        "\"Fira Code\", \"FiraCode Nerd Font\", \"Cascadia Code\", Consolas, monospace",
    ),
    (
        "Cascadia Mono",
        "\"Cascadia Mono\", \"Cascadia Code\", \"Cascadia Mono NF\", Consolas, monospace",
    ),
    (
        "Cascadia Code",
        "\"Cascadia Code\", \"Cascadia Code NF\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "MesloLGS NF",
        "\"MesloLGS NF\", \"MesloLGS Nerd Font\", \"MesloLGM Nerd Font\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "Hack",
        "\"Hack\", \"Hack Nerd Font\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "Source Code Pro",
        "\"Source Code Pro\", \"SauceCodePro Nerd Font\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "Iosevka Term",
        "\"Iosevka Term\", \"IosevkaTerm Nerd Font\", \"Iosevka\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "IBM Plex Mono",
        "\"IBM Plex Mono\", \"BlexMono Nerd Font\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "Inconsolata",
        "\"Inconsolata\", \"Inconsolata Nerd Font\", \"Cascadia Mono\", Consolas, monospace",
    ),
    (
        "Ubuntu Mono",
        "\"Ubuntu Mono\", \"UbuntuMono Nerd Font\", \"Cascadia Mono\", Consolas, monospace",
    ),
    ("Consolas", "Consolas, \"Cascadia Mono\", monospace"),
    ("Lucida Console", "\"Lucida Console\", Consolas, monospace"),
    ("Courier New", "\"Courier New\", Consolas, monospace"),
];

impl PreferencesPanel {
    /// 初始化 Kit 数值输入器的边界与步进，使键盘输入、失焦修正和加减按钮共用 main 的范围。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn configure_settings_number_inputs(
        inputs: &SettingsInputs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        configure_number(&inputs.font_size, 10., 24., 1., window, cx);
        configure_number(&inputs.line_height, 1., 1.8, 0.05, window, cx);
        configure_number(&inputs.scrollback, 1_000., 50_000., 500., window, cx);
        configure_number(&inputs.sftp_global_transfers, 1., 16., 1., window, cx);
        configure_number(&inputs.sftp_host_transfers, 1., 8., 1., window, cx);
        configure_number(&inputs.sftp_pipeline_depth, 1., 256., 1., window, cx);
        configure_number(
            &inputs.sftp_packet_bytes,
            0.03125,
            0.25,
            0.03125,
            window,
            cx,
        );
        configure_number(&inputs.sftp_idle_timeout, 30., 3_600., 1., window, cx);
    }

    /// 先展开并通知再等待搜索定位，避免直接在 render 中修改持久折叠状态。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn reveal_terminal_renderer(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if !self.terminal_renderer_open {
            self.terminal_renderer_open = true;
            cx.notify();
        }
    }

    /// 按 main 的断点组合终端设置，并依据明暗策略选择终端预览，避免应用色板干扰终端。
    ///
    /// @author kongweiguang
    pub(super) fn render_terminal(
        &mut self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return self.render_settings_loading(theme).into_any_element();
        };
        let locked = self.settings_controls_locked();
        let terminal = settings.terminal;
        let viewport_width = window.viewport_size().width.as_f32();
        let medium_layout = viewport_width >= 768.0;
        let large_layout = viewport_width >= 1024.0;
        let wide_layout = viewport_width >= 1280.0;
        let is_dark = match settings.theme_mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => matches!(
                window.appearance(),
                gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
            ),
        };
        let resolved_font_family =
            TerminalMetrics::from_settings(&terminal, &window.text_system().all_font_names())
                .font_family;

        let font_weight_options = vec![
            (TerminalFontWeight::Normal, "常规".into()),
            (TerminalFontWeight::Medium, "中等".into()),
            (TerminalFontWeight::Bold, "加粗".into()),
        ];
        let right_click_options = vec![
            (TerminalRightClickBehavior::None, "不执行".into()),
            (TerminalRightClickBehavior::Paste, "粘贴".into()),
            (TerminalRightClickBehavior::Menu, "菜单".into()),
        ];

        let font_controls = vec![
            div()
                .w_full()
                .min_w(px(0.))
                .child(settings_dropdown(
                    SettingsDropdownSpec {
                        id: "terminal-font-family",
                        label: crate::locale::text("终端字体", "Terminal font"),
                        accessibility_label: crate::locale::text("终端字体", "Terminal font"),
                        current_label: current_font_label(&terminal.font_family),
                        current: terminal.font_family.clone(),
                        options: FONT_OPTIONS
                            .iter()
                            .map(|(label, value)| ((*value).to_owned(), (*label).into()))
                            .collect(),
                        update: set_font_family,
                    },
                    locked,
                    theme,
                    cx,
                ))
                .into_any_element(),
            number_setting_field(
                crate::locale::text("字号", "Font size"),
                &self.settings_inputs.font_size,
                Some(crate::locale::text("px", "px")),
                None,
                true,
                locked,
                theme,
            )
            .into_any_element(),
            number_setting_field(
                crate::locale::text("行高", "Line height"),
                &self.settings_inputs.line_height,
                None,
                None,
                true,
                locked,
                theme,
            )
            .into_any_element(),
        ];
        let font_weight_control = settings_dropdown(
            SettingsDropdownSpec {
                id: "terminal-font-weight",
                label: crate::locale::text("字重", "Font weight"),
                accessibility_label: crate::locale::text("终端字重", "Terminal weight"),
                current_label: font_weight_label(terminal.font_weight),
                current: terminal.font_weight,
                options: font_weight_options,
                update: set_font_weight,
            },
            locked,
            theme,
            cx,
        )
        .into_any_element();
        let font_grid = super::render_settings::settings_grid_rows(
            font_controls,
            if medium_layout {
                &[2.0, 1.0, 1.0]
            } else {
                &[1.0]
            },
            super::render_settings::SettingsGridGap::Standard,
            false,
        );

        let interaction_controls = vec![
            policy_switch(
                "terminal-selection-copy",
                IconName::MousePointerClick,
                crate::locale::text("选中复制", "Copy selection"),
                terminal.selection_copy,
                locked,
                theme,
                cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(|settings| settings.terminal.selection_copy = *enabled, cx);
                }),
            )
            .into_any_element(),
            policy_switch(
                "terminal-show-tab-numbers",
                IconName::Terminal,
                crate::locale::text("显示标签序号", "Show tab numbers"),
                terminal.show_tab_numbers,
                locked,
                theme,
                cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| settings.terminal.show_tab_numbers = *enabled,
                        cx,
                    );
                }),
            )
            .into_any_element(),
            policy_switch(
                "terminal-cursor-blink",
                IconName::Type,
                crate::locale::text("光标闪烁", "Cursor blink"),
                terminal.cursor_blink,
                locked,
                theme,
                cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(|settings| settings.terminal.cursor_blink = *enabled, cx);
                }),
            )
            .into_any_element(),
            policy_switch(
                "terminal-command-rail",
                IconName::Terminal,
                crate::locale::text("显示命令色条", "Show command rail"),
                terminal.show_command_block_rail,
                locked,
                theme,
                cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| settings.terminal.show_command_block_rail = *enabled,
                        cx,
                    );
                }),
            )
            .into_any_element(),
            policy_switch(
                "terminal-confirm-close",
                IconName::RotateCcw,
                crate::locale::text("关闭标签前确认", "Confirm before closing tab"),
                terminal.confirm_close_tab,
                locked,
                theme,
                cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| settings.terminal.confirm_close_tab = *enabled,
                        cx,
                    );
                }),
            )
            .into_any_element(),
            policy_switch(
                "terminal-auto-reconnect",
                IconName::RotateCcw,
                crate::locale::text("自动重连", "Reconnect automatically"),
                terminal.auto_reconnect,
                locked,
                theme,
                cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(|settings| settings.terminal.auto_reconnect = *enabled, cx);
                }),
            )
            .into_any_element(),
        ];
        let interaction_grid = super::render_settings::settings_grid_rows(
            interaction_controls,
            if wide_layout {
                &[1.0, 1.0, 1.0]
            } else if medium_layout {
                &[1.0, 1.0]
            } else {
                &[1.0]
            },
            super::render_settings::SettingsGridGap::Compact,
            false,
        );

        let behavior_controls = vec![
            div()
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .font_weight(theme.settings_medium_font_weight())
                        .text_color(theme.field_label)
                        .child(crate::locale::text("键盘", "Keyboard")),
                )
                .child(policy_switch(
                    "terminal-mac-option-meta",
                    IconName::Terminal,
                    crate::locale::text(
                        "将 macOS Option 键作为 Meta 键",
                        "Use macOS Option as Meta",
                    ),
                    terminal.mac_option_is_meta,
                    locked,
                    theme,
                    cx.listener(|panel, enabled: &bool, _, cx| {
                        panel.edit_settings(
                            |settings| settings.terminal.mac_option_is_meta = *enabled,
                            cx,
                        );
                    }),
                ))
                .into_any_element(),
            settings_dropdown(
                SettingsDropdownSpec {
                    id: "terminal-right-click",
                    label: crate::locale::text("右键行为", "Right-click behavior"),
                    accessibility_label: crate::locale::text(
                        "终端右键行为",
                        "Terminal right-click behavior",
                    ),
                    current_label: right_click_label(terminal.right_click_behavior),
                    current: terminal.right_click_behavior,
                    options: right_click_options,
                    update: set_right_click,
                },
                locked,
                theme,
                cx,
            )
            .into_any_element(),
            number_setting(
                crate::locale::text("滚屏缓冲", "Scrollback"),
                &self.settings_inputs.scrollback,
                Some(crate::locale::text("行", "lines")),
                None,
                locked,
                theme,
            )
            .into_any_element(),
        ];
        let behavior_grid = super::render_settings::settings_grid_rows(
            behavior_controls,
            if wide_layout {
                &[0.9, 1.25, 1.0]
            } else if medium_layout {
                &[1.0, 1.0]
            } else {
                &[1.0]
            },
            super::render_settings::SettingsGridGap::Standard,
            true,
        );

        let content = div()
            .id("preferences-terminal")
            .flex_1()
            .w_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .w_full()
                    .flex()
                    .when(wide_layout, |layout| layout.flex_row().items_stretch())
                    .when(!wide_layout, |layout| layout.flex_col())
                    .gap_4()
                    .child(
                        subpanel(
                            crate::locale::text("终端主题", "Terminal theme"),
                            Some(IconName::Terminal),
                            theme,
                        )
                        .when(wide_layout, |panel| {
                            panel.flex_grow(0.9).flex_basis(px(0.)).min_w(px(0.))
                        })
                        .child(scheme_picker(
                            "terminal-scheme-light",
                            crate::locale::text("浅色终端主题", "Light terminal theme"),
                            terminal.light_color_scheme,
                            false,
                            locked,
                            theme,
                            cx,
                        ))
                        .child(scheme_picker(
                            "terminal-scheme-dark",
                            crate::locale::text("暗色终端主题", "Dark terminal theme"),
                            terminal.dark_color_scheme,
                            true,
                            locked,
                            theme,
                            cx,
                        )),
                    )
                    .child(
                        subpanel(
                            crate::locale::text("字体配置", "Font settings"),
                            Some(IconName::Type),
                            theme,
                        )
                        .when(wide_layout, |panel| {
                            panel.flex_grow(1.1).flex_basis(px(0.)).min_w(px(0.))
                        })
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(font_grid)
                                .child(font_weight_control),
                        )
                        .child(font_preview(
                            &terminal,
                            is_dark,
                            resolved_font_family,
                            theme,
                        )),
                    ),
            )
            .child(renderer_disclosure(
                self.terminal_renderer_open,
                locked,
                theme,
                cx,
            ))
            .child(
                subpanel(
                    crate::locale::text("终端交互", "Terminal interaction"),
                    Some(IconName::MousePointerClick),
                    theme,
                )
                .child(interaction_grid)
                .child(behavior_grid),
            )
            .child(cursor_section(
                &terminal,
                is_dark,
                locked,
                large_layout,
                theme,
                cx,
            ))
            .children(
                self.settings_issues
                    .iter()
                    .map(|issue| super::render_settings::settings_issue_row(issue, theme)),
            )
            .children(
                self.settings_input_errors
                    .values()
                    .map(|message| super::render_settings::issue_row(message, theme)),
            )
            .children(super::render::settings_error_banner(
                self.settings_error.as_deref(),
                theme,
            ))
            .child(self.render_save_notice(theme));

        content.into_any_element()
    }
}

/// 为同一组输入器套用 min/max/step；状态本身负责把步进与失焦修正限制在范围内。
///
/// @author kongweiguang
fn configure_number(
    input: &Entity<InputState>,
    min: f64,
    max: f64,
    step: f64,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    input.update(cx, |state, cx| {
        state.set_min(Some(min), window, cx);
        state.set_max(Some(max), window, cx);
        state.set_step(Some(NumberStep::Fixed(step)), window, cx);
    });
}

/// 卡片沿用 main 的分组表面；无图标的光标分区使用较轻的小标题，保留原有层级。
///
/// @author kongweiguang
fn subpanel(
    title: impl Into<SharedString>,
    icon: Option<IconName>,
    theme: ThemeTokens,
) -> gpui_kit::Div {
    div()
        .w_full()
        .min_w(px(0.))
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .bg(theme.panel)
        .rounded(theme.radius_panel)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .when_some(icon, |row, icon| {
                    row.child(
                        gpui_kit::component::Icon::new(icon)
                            .size(px(16.))
                            .text_color(theme.muted),
                    )
                })
                .child(
                    div()
                        .text_size(if icon.is_some() {
                            theme.text_sm
                        } else {
                            theme.text_xs
                        })
                        .font_weight(theme.settings_medium_font_weight())
                        .text_color(if icon.is_some() {
                            theme.foreground
                        } else {
                            theme.field_label
                        })
                        .child(title.into()),
                ),
        )
}

/// 复刻 main 的两列配色卡；窄列中压缩色样与勾选标记，给方案标题留足宽度。
///
/// @author kongweiguang
fn scheme_picker(
    id_prefix: &'static str,
    label: SharedString,
    current: TerminalColorScheme,
    dark_scheme: bool,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.field_label)
                .child(label),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .children(SCHEME_OPTIONS.chunks(2).map(|row_options| {
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(row_options.iter().copied().map(|option| {
                            let selected = option.value == current;
                            let option_value = option.value;
                            let fill = if selected {
                                theme.surface_selected()
                            } else {
                                theme.secondary
                            };
                            let border = if selected {
                                theme.tunnel_action.opacity(0.45)
                            } else {
                                theme.border
                            };
                            let hover = if selected {
                                theme.surface_selected()
                            } else {
                                theme.surface_hover()
                            };
                            Button::new(format!("{id_prefix}-{}", option.id))
                                .flex_1()
                                .flex_basis(px(0.))
                                .min_w(px(0.))
                                .min_h(px(96.))
                                .h_auto()
                                .p_3()
                                .rounded(theme.control_radius)
                                .outline()
                                .border_1()
                                .border_color(border)
                                .selected(selected)
                                .toggled(selected)
                                .accessibility_label(crate::locale::text(
                                    format!("{}：{}", option.label, option.description),
                                    format!("{}: {}", option.label, option.description),
                                ))
                                .tab_stop(true)
                                .tab_index(0)
                                .focus_ring(true)
                                .disabled(disabled)
                                .custom(
                                    ButtonCustomVariant::new(cx)
                                        .color(fill)
                                        .foreground(theme.foreground)
                                        .hover(hover)
                                        .active(hover),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .w_full()
                                                .min_w(px(0.))
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .min_w(px(0.))
                                                        .flex_1()
                                                        .truncate()
                                                        .text_size(theme.text_sm)
                                                        .font_weight(
                                                            theme.settings_medium_font_weight(),
                                                        )
                                                        .text_color(if selected {
                                                            theme.selected_foreground
                                                        } else {
                                                            theme.muted
                                                        })
                                                        .child(option.label),
                                                )
                                                .child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .flex()
                                                        .items_center()
                                                        .gap_1()
                                                        .child(
                                                            div()
                                                                .flex()
                                                                .overflow_hidden()
                                                                .rounded_full()
                                                                .border_1()
                                                                .border_color(
                                                                    theme.border.opacity(0.5),
                                                                )
                                                                .children(
                                                                    option
                                                                        .swatches
                                                                        .into_iter()
                                                                        .map(|color| {
                                                                            div().size(px(12.)).bg(
                                                                                gpui_kit::rgb(
                                                                                    color,
                                                                                ),
                                                                            )
                                                                        }),
                                                                ),
                                                        )
                                                        .when(selected, |row| {
                                                            row.child(
                                                            div()
                                                                .size(px(16.))
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .rounded_full()
                                                                .bg(theme.tunnel_action)
                                                                .text_color(theme.background)
                                                                .child(
                                                                    gpui_kit::component::Icon::new(
                                                                        IconName::Check,
                                                                    )
                                                                        .size(px(10.)),
                                                                ),
                                                        )
                                                        }),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .w_full()
                                                .min_w(px(0.))
                                                .text_size(theme.text_xs)
                                                .line_height(px(20.))
                                                .whitespace_normal()
                                                .text_color(theme.muted.opacity(0.8))
                                                .text_left()
                                                .child(option.description),
                                        ),
                                )
                                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                    panel.edit_settings(
                                        move |settings| {
                                            set_terminal_scheme(
                                                settings,
                                                option_value,
                                                dark_scheme,
                                            );
                                        },
                                        cx,
                                    );
                                }))
                        }))
                })),
        )
}

/// 终端 scheme 同步当前选择与对应的明暗字段，切换桌面主题时保留另一侧偏好。
///
/// @author kongweiguang
fn set_terminal_scheme(
    settings: &mut SettingsSnapshot,
    value: TerminalColorScheme,
    dark_scheme: bool,
) {
    settings.terminal.color_scheme = value;
    if dark_scheme {
        settings.terminal.dark_color_scheme = value;
    } else {
        settings.terminal.light_color_scheme = value;
    }
}

/// 字段标题与 NumberInput 同组显示，维持其他设置数字控件原有的最小宽度。
///
/// @author kongweiguang
pub(in crate::features::preferences) fn number_setting(
    label: SharedString,
    input: &Entity<InputState>,
    suffix: Option<SharedString>,
    help: Option<SharedString>,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    number_setting_field(label, input, suffix, help, false, disabled, theme)
}

/// 数字输入沿用 main 的普通字段而不增加步进按钮；保留原 InputState 的范围与中间态校验。
/// 字号与行高并排时压缩最小宽度，36px 命中高度保持一致。
///
/// @author kongweiguang
fn number_setting_field(
    label: SharedString,
    input: &Entity<InputState>,
    suffix: Option<SharedString>,
    help: Option<SharedString>,
    compact_controls: bool,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    let field = gpui_kit::Styled::h(
        super::render_settings::settings_input(input, theme)
            .when(compact_controls, |field| field.small()),
        px(36.),
    )
    .w_full()
    .disabled(disabled)
    .when_some(suffix, |field, suffix| field.suffix(suffix));
    div()
        .flex_1()
        .min_w(px(if compact_controls { 96. } else { 112. }))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.field_label)
                .child(label),
        )
        .child(field)
        .when_some(help, |field, help| {
            field.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(help),
            )
        })
}

/// 显示 main 对应的字段标签和尾部箭头，并让当前值与可访问名称保持分离。
///
/// @author kongweiguang
fn settings_dropdown<T>(
    spec: SettingsDropdownSpec<T>,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement
where
    T: Clone + PartialEq + 'static,
{
    let SettingsDropdownSpec {
        id,
        label,
        accessibility_label,
        current_label,
        current,
        options,
        update,
    } = spec;
    let entity = cx.entity().clone();
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.field_label)
                .child(label),
        )
        .child(
            Button::new(id)
                .compact()
                .h(px(36.))
                .rounded(theme.control_radius)
                .border_1()
                .border_color(theme.strong_border())
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(theme.field_surface())
                        .foreground(theme.foreground)
                        .hover(theme.field_hover_surface())
                        .active(theme.field_hover_surface()),
                )
                .w_full()
                .min_w(px(0.))
                .label(current_label)
                .dropdown_caret(true)
                .accessibility_label(accessibility_label)
                .disabled(disabled)
                .dropdown_menu(move |mut menu, _, _| {
                    menu = crate::ui_popup::dropdown_menu(menu, theme);
                    for (value, label) in &options {
                        let selected = value == &current;
                        let value = value.clone();
                        let entity = entity.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label.clone())
                                .checked(selected)
                                .on_click(move |_, _, app| {
                                    entity.update(app, |panel, cx| {
                                        panel.edit_settings(
                                            |settings| update(settings, value.clone()),
                                            cx,
                                        );
                                    });
                                }),
                        );
                    }
                    menu
                }),
        )
}

/// 将字体显示名和可跨平台回退的 CSS 栈写入终端设置快照。
///
/// @author kongweiguang
fn set_font_family(settings: &mut SettingsSnapshot, value: String) {
    settings.terminal.font_family = value;
}

/// 把主终端字体族列表映射成当前预览可见的显示名，未列出的旧值仍可读。
///
/// @author kongweiguang
fn current_font_label(value: &str) -> SharedString {
    FONT_OPTIONS
        .iter()
        .find(|(_, family)| *family == value)
        .map(|(label, _)| (*label).into())
        .unwrap_or_else(|| primary_font_family(value).into())
}

/// 从字体回退栈取首选族给预览使用，同时保留原始完整栈用于持久化。
///
/// @author kongweiguang
fn primary_font_family(value: &str) -> String {
    value
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_owned()
}

/// 将用户选择的字重写入 snapshot，字体预览和运行时终端读取同一字段。
///
/// @author kongweiguang
fn set_font_weight(settings: &mut SettingsSnapshot, value: TerminalFontWeight) {
    settings.terminal.font_weight = value;
}

/// 映射应用的字体字重枚举到预览文字的 Kit 字重。
///
/// @author kongweiguang
fn font_weight(value: TerminalFontWeight) -> gpui_kit::FontWeight {
    match value {
        TerminalFontWeight::Normal => gpui_kit::FontWeight::NORMAL,
        TerminalFontWeight::Medium => gpui_kit::FontWeight::MEDIUM,
        TerminalFontWeight::Bold => gpui_kit::FontWeight::BOLD,
    }
}

/// 预览沿用终端布局解析出的已安装字体，并保留 main 的内容表面与字段字级，避免缺失首选字体时预览和实际绘制分离。
///
/// @author kongweiguang
fn font_preview(
    terminal: &kerminal_application::preferences::TerminalSettings,
    is_dark: bool,
    resolved_font_family: SharedString,
    theme: ThemeTokens,
) -> impl IntoElement {
    let scheme = if is_dark {
        terminal.dark_color_scheme
    } else {
        terminal.light_color_scheme
    };
    let colors = terminal_preview_colors(scheme, is_dark);
    let font_size = px(f32::from(terminal.font_size));
    let line_height = px(f32::from(terminal.font_size) * terminal.line_height as f32);
    let font_weight = font_weight(terminal.font_weight);
    div()
        .id("terminal-font-preview")
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.field_label)
                .child(crate::locale::text("字体预览", "Font preview")),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap_2()
                .rounded(theme.control_radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .px_3()
                .py_2()
                .child(
                    div()
                        .w(px(40.))
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme.control_radius)
                        .border_1()
                        .border_color(theme.border)
                        .font_family(resolved_font_family.clone())
                        .font_weight(font_weight)
                        .text_size(theme.text_sm)
                        .line_height(px(20.))
                        .child("Aa"),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .line_height(px(16.))
                                .text_color(theme.muted)
                                .child(crate::locale::text("当前预览", "Current preview")),
                        )
                        .child(
                            div()
                                .text_size(theme.text_sm)
                                .line_height(px(20.))
                                .font_weight(theme.settings_medium_font_weight())
                                .text_color(theme.foreground)
                                .child(current_font_label(&terminal.font_family)),
                        ),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .overflow_hidden()
                .rounded(theme.radius_sm)
                .border_1()
                .border_color(theme.border)
                .bg(gpui_kit::rgb(colors.background))
                .text_color(gpui_kit::rgb(colors.foreground))
                .px_3()
                .py_3()
                .children([
                    preview_line(
                        format!("font: {}", current_font_label(&terminal.font_family)),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        font_weight,
                        gpui_kit::rgb(colors.foreground).into(),
                    ),
                    preview_line(
                        "abcdefghijklmnopqrstuvwxyz".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        font_weight,
                        gpui_kit::rgb(colors.foreground).into(),
                    ),
                    preview_line(
                        "0123456789 !@#$%^&*()[]{}<>".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        font_weight,
                        gpui_kit::rgb(colors.foreground).into(),
                    ),
                    preview_line(
                        "中文路径 /var/log/应用 日志输出".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        font_weight,
                        gpui_kit::rgb(colors.foreground).into(),
                    ),
                    preview_line(
                        "$ ssh root@server.example.com".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        font_weight,
                        gpui_kit::rgb(colors.green).into(),
                    ),
                    preview_line(
                        "$ npm run build && npm run start".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        font_weight,
                        gpui_kit::rgb(colors.cyan).into(),
                    )
                    .flex()
                    .items_end()
                    .child(cursor_marker(
                        terminal.cursor_style,
                        colors.cursor,
                        terminal.cursor_blink,
                        "terminal-font-preview-cursor",
                    )),
                ]),
        )
}

/// 预览行共用终端解析后的字体、字号和绝对行高，保持回退字体与尺寸行为一致。
///
/// @author kongweiguang
fn preview_line(
    text: String,
    family: &SharedString,
    font_size: gpui_kit::Pixels,
    line_height: gpui_kit::Pixels,
    weight: gpui_kit::FontWeight,
    color: gpui_kit::Hsla,
) -> gpui_kit::Div {
    div()
        .w_full()
        .min_w(px(0.))
        .overflow_hidden()
        .font_family(family.clone())
        .font_weight(weight)
        .text_size(font_size)
        .line_height(line_height)
        .text_color(color)
        .child(text)
}

/// 光标预览遵循当前闪烁设置，并通过 GPUI 重复动画尊重系统减少动态效果偏好。
///
/// @author kongweiguang
fn cursor_marker(
    style: TerminalCursorStyle,
    color: u32,
    blink: bool,
    animation_id: &'static str,
) -> impl IntoElement {
    let marker = div().bg(gpui_kit::rgb(color));
    let marker = match style {
        TerminalCursorStyle::Block => marker.w(px(8.)).h(px(15.)),
        TerminalCursorStyle::Bar => marker.w(px(2.)).h(px(16.)),
        TerminalCursorStyle::Underline => marker.w(px(8.)).h(px(2.)).mt(px(13.)),
    };
    if blink {
        marker
            .with_animation(
                animation_id,
                Animation::new(Duration::from_secs(2))
                    .repeat_synced()
                    .with_max_fps(30.),
                |marker, phase| {
                    let pulse = 0.5 - 0.5 * (std::f32::consts::TAU * phase).cos();
                    marker.opacity(1. - pulse * 0.5)
                },
            )
            .into_any_element()
    } else {
        marker.into_any_element()
    }
}

/// 预览色值与 main 的 xtermThemeFor 一致，并供终端与关键词预览共用，避免维护重复色盘。
///
/// @author kongweiguang
pub(in crate::features::preferences) fn terminal_preview_colors(
    scheme: TerminalColorScheme,
    is_dark: bool,
) -> TerminalPreviewColors {
    match (scheme, is_dark) {
        (TerminalColorScheme::Kerminal, false) => TerminalPreviewColors {
            background: 0xf7f7fa,
            foreground: 0x1d1d1f,
            green: 0x248a3d,
            cyan: 0x007aff,
            cursor: 0x1d1d1f,
        },
        (TerminalColorScheme::Kerminal, true) => TerminalPreviewColors {
            background: 0x1f1f21,
            foreground: 0xe4e4e7,
            green: 0x4ade80,
            cyan: 0x22d3ee,
            cursor: 0xf8fafc,
        },
        (TerminalColorScheme::TokyoNight, false) => TerminalPreviewColors {
            background: 0xe1e2e7,
            foreground: 0x343b58,
            green: 0x485e30,
            cyan: 0x166775,
            cursor: 0x343b58,
        },
        (TerminalColorScheme::TokyoNight, true) => TerminalPreviewColors {
            background: 0x1a1b26,
            foreground: 0xc0caf5,
            green: 0x9ece6a,
            cyan: 0x2ac3de,
            cursor: 0xc0caf5,
        },
        (TerminalColorScheme::Solarized, false) => TerminalPreviewColors {
            background: 0xfdf6e3,
            foreground: 0x657b83,
            green: 0x859900,
            cyan: 0x2aa198,
            cursor: 0x586e75,
        },
        (TerminalColorScheme::Solarized, true) => TerminalPreviewColors {
            background: 0x002b36,
            foreground: 0x839496,
            green: 0x859900,
            cyan: 0x2aa198,
            cursor: 0x93a1a1,
        },
        (TerminalColorScheme::Github, false) => TerminalPreviewColors {
            background: 0xffffff,
            foreground: 0x24292f,
            green: 0x1a7f37,
            cyan: 0x1b7c83,
            cursor: 0x24292f,
        },
        (TerminalColorScheme::Github, true) => TerminalPreviewColors {
            background: 0x0d1117,
            foreground: 0xc9d1d9,
            green: 0x3fb950,
            cyan: 0x39c5cf,
            cursor: 0xf0f6fc,
        },
    }
}

/// 光标样式预览读取对应明暗模式的终端色盘，确保界面色板不会影响终端内容示例。
///
/// @author kongweiguang
fn cursor_section(
    terminal: &kerminal_application::preferences::TerminalSettings,
    is_dark: bool,
    locked: bool,
    large_layout: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    let scheme = if is_dark {
        terminal.dark_color_scheme
    } else {
        terminal.light_color_scheme
    };
    let colors = terminal_preview_colors(scheme, is_dark);
    let choices = [
        (
            TerminalCursorStyle::Block,
            "cursor-block",
            "块状",
            "醒目，易定位。",
        ),
        (
            TerminalCursorStyle::Bar,
            "cursor-bar",
            "竖线",
            "轻量插入点。",
        ),
        (
            TerminalCursorStyle::Underline,
            "cursor-underline",
            "下划线",
            "占用最少。",
        ),
    ];
    let cards = choices
        .into_iter()
        .map(|(style, id, label, description)| {
            let animation_id = match style {
                TerminalCursorStyle::Block => "terminal-cursor-preview-block",
                TerminalCursorStyle::Bar => "terminal-cursor-preview-bar",
                TerminalCursorStyle::Underline => "terminal-cursor-preview-underline",
            };
            let selected = terminal.cursor_style == style;
            Button::new(format!("terminal-{id}"))
                .flex_1()
                .min_w(px(160.))
                .h_auto()
                .p(px(14.))
                .rounded(theme.control_radius)
                .border_1()
                .border_color(if selected {
                    theme.accent.opacity(0.30)
                } else {
                    theme.border
                })
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(if selected {
                            theme.surface_selected()
                        } else {
                            theme.muted_surface()
                        })
                        .foreground(theme.settings_choice_foreground(selected))
                        .hover(theme.surface_hover())
                        .active(theme.surface_selected()),
                )
                .disabled(locked)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_2()
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_size(theme.text_sm).child(label))
                                .when(selected, |row| {
                                    row.child(gpui_kit::component::Icon::new(IconName::Check))
                                }),
                        )
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .min_h(px(40.))
                                .line_height(px(20.))
                                .text_color(theme.muted)
                                .child(crate::locale::text(description, description)),
                        )
                        .child(cursor_preview(
                            style,
                            terminal.cursor_blink,
                            animation_id,
                            colors,
                            theme,
                        )),
                )
                .accessibility_label(crate::locale::text(
                    format!("{label}光标：{description}"),
                    format!("{label} cursor: {description}"),
                ))
                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                    panel.edit_settings(move |settings| settings.terminal.cursor_style = style, cx);
                }))
                .into_any_element()
        })
        .collect();
    let cards = super::render_settings::settings_grid_rows(
        cards,
        if large_layout {
            &[1.0, 1.0, 1.0]
        } else {
            &[1.0]
        },
        super::render_settings::SettingsGridGap::Standard,
        false,
    );
    subpanel(crate::locale::text("光标形态", "Cursor style"), None, theme).child(cards)
}

/// 标题栏使用应用语义色，正文及光标从终端预览色盘读取，动画身份保持稳定。
///
/// @author kongweiguang
fn cursor_preview(
    style: TerminalCursorStyle,
    blink: bool,
    animation_id: &'static str,
    colors: TerminalPreviewColors,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .overflow_hidden()
        .rounded(theme.radius_sm)
        .border_1()
        .border_color(theme.border)
        .bg(gpui_kit::rgb(colors.background))
        .child(
            div()
                .w_full()
                .h(px(28.))
                .flex()
                .items_center()
                .gap_1()
                .px_3()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .children([
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(theme.error.opacity(0.8)),
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(theme.warning.opacity(0.8)),
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(theme.success.opacity(0.8)),
                ])
                .child(
                    div()
                        .ml_2()
                        .text_size(px(10.))
                        .font_weight(theme.settings_medium_font_weight())
                        .text_color(theme.muted)
                        .child("PROMPT"),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .px_3()
                .py_2()
                .font_family("monospace")
                .text_size(px(11.))
                .line_height(px(20.))
                .text_color(gpui_kit::rgb(colors.foreground))
                .child(
                    div()
                        .w_full()
                        .text_color(gpui_kit::rgb(colors.green))
                        .child("root@kerminal"),
                )
                .child(
                    div()
                        .w_full()
                        .text_color(gpui_kit::rgb(colors.foreground).opacity(0.75))
                        .child("~/workspace"),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_end()
                        .child(
                            div()
                                .text_color(gpui_kit::rgb(colors.cyan))
                                .child("$ edit cursor"),
                        )
                        .child(cursor_marker(style, colors.cursor, blink, animation_id)),
                ),
        )
}

/// Disclosure 保持 main 的 44px 标题命中区；禁止滚动内容挤压带裁切的容器，避免入口消失。
///
/// @author kongweiguang
fn renderer_disclosure(
    expanded: bool,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    let description = crate::locale::text(
        "终端由当前 GPUI 窗口的原生渲染器绘制。",
        "Terminals are drawn by the native renderer in the current GPUI window.",
    );
    let status = crate::locale::text("GPUI 原生渲染", "GPUI native rendering");
    div()
        .w_full()
        .flex_shrink_0()
        .min_h(px(46.))
        .overflow_hidden()
        .rounded(theme.radius_panel)
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .flex()
        .flex_col()
        .child(
            Button::new("terminal-renderer-disclosure")
                .compact()
                .custom(super::render_settings::settings_ghost_variant(theme, cx))
                .w_full()
                .h(px(44.))
                .accessibility_label(crate::locale::text("终端渲染", "Terminal rendering"))
                .disabled(disabled)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(gpui_kit::component::Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                }))
                                .child(
                                    div()
                                        .text_size(theme.text_sm)
                                        .font_weight(theme.settings_medium_font_weight())
                                        .text_color(theme.foreground)
                                        .child(crate::locale::text(
                                            "终端渲染",
                                            "Terminal rendering",
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(status),
                        ),
                )
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                    panel.terminal_renderer_open = !panel.terminal_renderer_open;
                    cx.notify();
                })),
        )
        .when(expanded, |body| {
            body.child(
                div()
                    .w_full()
                    .border_t_1()
                    .border_color(theme.border)
                    .p_3()
                    .child(
                        div()
                            .w_full()
                            .rounded(theme.control_radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .px_3()
                            .py_2()
                            .text_size(theme.text_xs)
                            .line_height(px(20.))
                            .text_color(theme.muted)
                            .child(description),
                    ),
            )
        })
}

/// 将 terminal font weight 变更提交到同一设置草稿，不派生第二份控件状态。
///
/// @author kongweiguang
fn font_weight_label(value: TerminalFontWeight) -> SharedString {
    match value {
        TerminalFontWeight::Normal => crate::locale::text("常规", "Normal"),
        TerminalFontWeight::Medium => crate::locale::text("中等", "Medium"),
        TerminalFontWeight::Bold => crate::locale::text("加粗", "Bold"),
    }
}

/// 右键操作保持 main 的三种互斥选择，避免菜单和粘贴同时触发。
///
/// @author kongweiguang
fn set_right_click(settings: &mut SettingsSnapshot, value: TerminalRightClickBehavior) {
    settings.terminal.right_click_behavior = value;
}

/// 将当前右键策略映射成明确的菜单按钮名称。
///
/// @author kongweiguang
fn right_click_label(value: TerminalRightClickBehavior) -> SharedString {
    match value {
        TerminalRightClickBehavior::None => crate::locale::text("不执行", "None"),
        TerminalRightClickBehavior::Paste => crate::locale::text("粘贴", "Paste"),
        TerminalRightClickBehavior::Menu => crate::locale::text("菜单", "Menu"),
    }
}

/// 行点击复刻 main 的整行开关热区，零最小宽度让 viewport 网格中的长标签换行。
/// 子 Switch 截断冒泡，保留 Kit 的受控、键盘与禁用语义。
///
/// @author kongweiguang
fn policy_switch(
    id: &'static str,
    icon: IconName,
    label: SharedString,
    checked: bool,
    disabled: bool,
    theme: ThemeTokens,
    listener: impl Fn(&bool, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    let switch_label = label.clone();
    let listener = std::rc::Rc::new(listener);
    let switch_listener = listener.clone();
    div()
        .id(format!("{id}-row"))
        .w_full()
        .min_w(px(0.))
        .min_h(px(44.))
        .rounded(theme.control_radius)
        .border_1()
        .border_color(if checked {
            theme.tunnel_action.opacity(0.35)
        } else {
            theme.border
        })
        .bg(if checked {
            theme.surface_selected()
        } else {
            theme.background
        })
        .px_3()
        .py_2()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap_2()
                .child(
                    gpui_kit::component::Icon::new(icon)
                        .size(px(16.))
                        .text_color(theme.muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(theme.text_sm)
                        .line_height(px(20.))
                        .whitespace_normal()
                        .text_left()
                        .text_color(theme.settings_choice_foreground(checked))
                        .child(label),
                ),
        )
        .child(
            Switch::new(id)
                .track_size(px(40.), px(24.))
                .checked(checked)
                .disabled(disabled)
                .accessibility_label(switch_label)
                .flex_shrink_0()
                .on_change(move |next, window, cx| {
                    cx.stop_propagation();
                    switch_listener(next, window, cx);
                }),
        )
        .when(!disabled, |row| {
            row.cursor_pointer().on_click(move |_, window, cx| {
                listener(&!checked, window, cx);
            })
        })
}
