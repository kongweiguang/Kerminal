//! 主题、界面、终端外观和快捷键的原生控件。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, IntoElement, SharedString, StatefulInteractiveElement,
    Window,
    assets::IconName,
    base::Selectable,
    component::{
        Disableable, FocusableExt as _, Icon, Sizable as _,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        input::{Input, InputState},
        select::Select,
        slider::{Slider, SliderState},
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::SettingsSnapshot;

use crate::theme::ThemeTokens;

use super::{
    BackgroundFit, InterfaceDensity, KeybindingScope, KeyboardPlatform, TerminalColorScheme,
    TerminalCursorStyle, ThemeMode,
    panel::{PreferencesPanel, ShortcutBindingField, ShortcutInputs, keyboard_platform},
    render::settings_error_banner,
    validate_settings,
};

const THEME_CHOICE_HEIGHT_PX: f32 = 60.0;
const DENSITY_SEGMENT_HEIGHT_PX: f32 = 44.0;
const DENSITY_SEGMENT_MIN_WIDTH_PX: f32 = 136.0;
// 460px 保留三个现有密度选项和卡片内边距；是否并排只由扣除侧栏后的实际正文宽度决定。
const APPEARANCE_COLUMN_BASIS_PX: f32 = 460.0;

/// 同一组密度选项共用标签、值与稳定 ID，避免分段按钮和保存回调各自维护不同顺序。
/// @author kongweiguang
#[derive(Clone, Copy)]
struct DensityChoiceSpec {
    id: &'static str,
    value: InterfaceDensity,
    label_zh: &'static str,
    label_en: &'static str,
}

const DENSITY_CHOICES: [DensityChoiceSpec; 3] = [
    DensityChoiceSpec {
        id: "density-compact",
        value: InterfaceDensity::Compact,
        label_zh: "紧凑",
        label_en: "Compact",
    },
    DensityChoiceSpec {
        id: "density-comfortable",
        value: InterfaceDensity::Comfortable,
        label_zh: "舒适",
        label_en: "Comfortable",
    },
    DensityChoiceSpec {
        id: "density-spacious",
        value: InterfaceDensity::Spacious,
        label_zh: "宽松",
        label_en: "Spacious",
    },
];

/// 将三个系统的快捷键输入并列呈现；Windows 备用键作为同一平台的小型第二行。
///
/// @author kongweiguang
fn shortcut_platform_cell(
    platform: SharedString,
    binding_label: SharedString,
    primary: Option<Entity<InputState>>,
    disabled: bool,
    theme: ThemeTokens,
    note: Option<AnyElement>,
    alternate: Option<(SharedString, Option<Entity<InputState>>)>,
) -> impl IntoElement {
    let mut cell = div()
        .flex_1()
        .min_w(px(190.))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_sm)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(platform),
        );
    if let Some(note) = note {
        cell = cell.child(note);
    }
    cell = cell.child(shortcut_input_row(binding_label, primary, disabled, theme));
    if let Some((label, input)) = alternate {
        cell = cell.child(shortcut_input_row(label, input, disabled, theme));
    }
    cell
}

/// 输入尚未加载时保留明确占位；标签与输入同一行以节省每条快捷键卡的高度。
///
/// @author kongweiguang
fn shortcut_input_row(
    label: SharedString,
    input: Option<Entity<InputState>>,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(58.))
                .flex_shrink_0()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(match input {
            Some(input) => Input::new(&input)
                .disabled(disabled)
                .w_full()
                .min_w(px(0.))
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text("加载中…", "Loading…"))
                .into_any_element(),
        })
}

/// 呈现 Linux 继承状态和显式禁用入口，避免空白输入掩盖旧配置的回退语义。
///
/// @author kongweiguang
fn linux_binding_controls(
    binding: &kerminal_application::preferences::KeybindingPreference,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> AnyElement {
    let inherited = binding.linux_binding.is_none();
    let note = match binding.linux_binding.as_deref() {
        None => {
            let value = effective_binding(&binding.windows_binding, &binding.binding);
            if value.is_empty() {
                crate::locale::text(
                    "沿用 Windows/通用值：未绑定",
                    "Inherited Windows/generic value: unbound",
                )
            } else {
                crate::locale::text(
                    format!("沿用 Windows/通用值：{value}"),
                    format!("Inherited Windows/generic value: {value}"),
                )
            }
        }
        Some("") => crate::locale::text("Linux 绑定已禁用", "Linux binding is disabled"),
        Some(_) => crate::locale::text("使用 Linux 独立绑定", "Using a Linux-specific binding"),
    };
    let action = binding.action.clone();
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(note),
        )
        .child(
            Button::new(format!("shortcut-linux-toggle-{}", binding.action))
                .compact()
                .ghost()
                .label(if inherited {
                    crate::locale::text("禁用 Linux", "Disable Linux")
                } else {
                    crate::locale::text("恢复继承", "Restore inherited")
                })
                .disabled(disabled)
                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                    panel.toggle_linux_binding_inheritance(&action, window, cx)
                })),
        )
        .into_any_element()
}

impl PreferencesPanel {
    /// 恢复、读取、写入和外部冲突期间锁定表单，避免视图与 CAS 草稿短暂分叉。
    pub(super) fn settings_controls_locked(&self) -> bool {
        self.loading_settings
            || self.recovery.mode
                == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery
            || self.settings.as_ref().is_some_and(|draft| {
                matches!(
                    draft.status(),
                    super::DraftStatus::Saving
                        | super::DraftStatus::ExternalConflict
                        | super::DraftStatus::OutcomeUnknown
                )
            })
    }

    /// 外观卡片按实际正文宽度自然换行，避免用窗口宽度误把侧导航算作可用内容。
    /// 只重排现有控件与次要入口；主题、密度和背景仍更新同一 typed 草稿及保存门禁。
    /// 关闭图片只收起其专属控件；输入与滑块实体、未保存值和独立材质透明度继续由原 owner 保留。
    ///
    /// @author kongweiguang
    pub(super) fn render_appearance(
        &mut self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return self.render_settings_loading(theme);
        };
        let controls_locked = self.settings_controls_locked();
        let mode = settings.theme_mode;
        let language = settings.interface_language;
        let density = settings.interface_density;
        let background_enabled = settings.background_enabled;
        let background_fit = settings.background_fit;
        sync_percentage_slider(
            &self.background_opacity_slider,
            settings.background_opacity,
            0.,
            100.,
            window,
            cx,
        );
        sync_percentage_slider(
            &self.window_opacity_slider,
            settings.window_opacity,
            35.,
            100.,
            window,
            cx,
        );
        if self
            .interface_language_select
            .read(cx)
            .selected_value()
            .copied()
            != Some(language)
        {
            self.interface_language_select.update(cx, |select, cx| {
                select.set_selected_value(&language, window, cx);
            });
        }
        let issue_rows = self
            .settings_issues
            .iter()
            .map(|issue| settings_issue_row(issue, theme));
        let input_errors = self
            .settings_input_errors
            .values()
            .map(|message| issue_row(message, theme));

        let mut body = div()
            .id("preferences-appearance")
            .flex_1()
            .w_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .p_4()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .content_start()
            .gap_4()
            .bg(theme.background)
            .child(
                section_card(crate::locale::text("基础外观", "Basic appearance"), theme)
                    .map(appearance_column)
                    .child(form_row(
                        crate::locale::text("界面语言", "Interface language"),
                        crate::locale::text(
                            "跟随系统会使用系统语言。",
                            "System mode follows the operating system language.",
                        ),
                        Select::new(&self.interface_language_select)
                            .w_full()
                            .h(px(36.))
                            .disabled(controls_locked)
                            .accessibility_label(crate::locale::text(
                                "界面语言",
                                "Interface language",
                            )),
                        theme,
                    ))
                    .child(form_row(
                        crate::locale::text("主题", "Theme"),
                        crate::locale::text("选择应用界面的外观。", "Choose the app appearance."),
                        div()
                            .w_full()
                            .flex()
                            .gap_3()
                            .child(theme_choice_card(
                                "theme-dark",
                                IconName::Moon,
                                crate::locale::text("深色", "Dark"),
                                ChoiceCardStyle {
                                    selected: mode == ThemeMode::Dark,
                                    disabled: controls_locked,
                                    theme,
                                },
                                cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.edit_settings(
                                        |settings| settings.theme_mode = ThemeMode::Dark,
                                        cx,
                                    )
                                }),
                                cx,
                            ))
                            .child(theme_choice_card(
                                "theme-light",
                                IconName::Sun,
                                crate::locale::text("浅色", "Light"),
                                ChoiceCardStyle {
                                    selected: mode == ThemeMode::Light,
                                    disabled: controls_locked,
                                    theme,
                                },
                                cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.edit_settings(
                                        |settings| settings.theme_mode = ThemeMode::Light,
                                        cx,
                                    )
                                }),
                                cx,
                            ))
                            .child(theme_choice_card(
                                "theme-system",
                                IconName::MonitorCog,
                                crate::locale::text("跟随系统", "System"),
                                ChoiceCardStyle {
                                    selected: mode == ThemeMode::System,
                                    disabled: controls_locked,
                                    theme,
                                },
                                cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.edit_settings(
                                        |settings| settings.theme_mode = ThemeMode::System,
                                        cx,
                                    )
                                }),
                                cx,
                            )),
                        theme,
                    ))
                    .child(form_row(
                        crate::locale::text("界面密度", "Interface density"),
                        crate::locale::text(
                            "紧凑显示更多，舒适更均衡，宽松增加留白。",
                            "Compact shows more; comfortable balances; spacious adds room.",
                        ),
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .p_1()
                            .rounded(theme.radius_sm)
                            .bg(theme.sidebar)
                            .children(DENSITY_CHOICES.into_iter().map(|choice| {
                                density_choice_segment(
                                    choice,
                                    ChoiceCardStyle {
                                        selected: density == choice.value,
                                        disabled: controls_locked,
                                        theme,
                                    },
                                    cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                        panel.edit_settings(
                                            |settings| settings.interface_density = choice.value,
                                            cx,
                                        )
                                    }),
                                    cx,
                                )
                            })),
                        theme,
                    ))
                    .child(
                        div().w_full().flex().justify_end().child(
                            Button::new("preferences-about-updates")
                                .compact()
                                .ghost()
                                .label(crate::locale::text("关于与更新", "About & updates"))
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.request_updates_navigation(cx);
                                })),
                        ),
                    ),
            )
            .child(
                section_card(
                    crate::locale::text("背景与窗口", "Background and window"),
                    theme,
                )
                .map(appearance_column)
                .child(
                    Switch::new("preferences-background-enabled")
                        .checked(background_enabled)
                        .disabled(controls_locked)
                        .label(crate::locale::text(
                            "启用背景图片",
                            "Enable background image",
                        ))
                        .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                            panel.edit_settings(
                                |settings| settings.background_enabled = *enabled,
                                cx,
                            )
                        })),
                )
                .children(appearance_image_controls(background_enabled, |controls| {
                    controls
                        .child(form_row(
                            crate::locale::text("背景图片路径", "Background image path"),
                            crate::locale::text(
                                "选择本机图片，路径会随设置草稿保存。",
                                "Choose a local image. Its path is saved with the settings draft.",
                            ),
                            div()
                                .w_full()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .items_start()
                                .gap_2()
                                .child(
                                    Input::new(&self.settings_inputs.background_path)
                                        .disabled(controls_locked)
                                        .w_full()
                                        .min_w(px(0.)),
                                )
                                .child(
                                    div().w_full().flex().justify_end().child(
                                        Button::new("preferences-choose-background")
                                            .compact()
                                            .flex_shrink_0()
                                            .label(crate::locale::text(
                                                "选择图片…",
                                                "Choose image…",
                                            ))
                                            .disabled(controls_locked)
                                            .on_click(cx.listener(
                                                |panel, _: &ClickEvent, _, cx| {
                                                    panel.choose_background_image(cx)
                                                },
                                            )),
                                    ),
                                ),
                            theme,
                        ))
                        .child(
                            enum_selector(
                                crate::locale::text("图片适配", "Image fit"),
                                Some(fit_label(background_fit)),
                                theme,
                            )
                            .child(enum_button(
                                "fit-cover",
                                crate::locale::text("填充", "Cover"),
                                background_fit == BackgroundFit::Cover,
                                controls_locked,
                                cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.edit_settings(
                                        |settings| settings.background_fit = BackgroundFit::Cover,
                                        cx,
                                    )
                                }),
                            ))
                            .child(enum_button(
                                "fit-contain",
                                crate::locale::text("完整显示", "Contain"),
                                background_fit == BackgroundFit::Contain,
                                controls_locked,
                                cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.edit_settings(
                                        |settings| settings.background_fit = BackgroundFit::Contain,
                                        cx,
                                    )
                                }),
                            ))
                            .child(enum_button(
                                "fit-tile",
                                crate::locale::text("平铺", "Tile"),
                                background_fit == BackgroundFit::Tile,
                                controls_locked,
                                cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.edit_settings(
                                        |settings| settings.background_fit = BackgroundFit::Tile,
                                        cx,
                                    )
                                }),
                            )),
                        )
                        .child(form_row(
                            crate::locale::text("背景透明度", "Image opacity"),
                            crate::locale::text(
                                "调整背景图片的可见程度。",
                                "Adjust how visible the background image is.",
                            ),
                            percentage_control(
                                &self.background_opacity_slider,
                                settings.background_opacity,
                                controls_locked,
                                theme,
                            ),
                            theme,
                        ))
                }))
                .child(form_row(
                    crate::locale::text("背景材质透明度", "Window material opacity"),
                    crate::locale::text(
                        "调整窗口与面板背景，文字保持清晰。",
                        "Adjusts window and panel backgrounds while keeping text clear.",
                    ),
                    percentage_control(
                        &self.window_opacity_slider,
                        settings.window_opacity,
                        controls_locked,
                        theme,
                    ),
                    theme,
                )),
            );
        if let Some(error) = settings_error_banner(self.settings_error.as_deref(), theme) {
            body = body.child(error);
        }
        body.children(issue_rows).children(input_errors)
    }

    /// 终端页字段纵向排列并填充可用宽度，避免固定输入宽度挤出设置卡片边界。
    ///
    /// @author kongweiguang
    pub(super) fn render_terminal(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return self.render_settings_loading(theme);
        };
        let controls_locked = self.settings_controls_locked();
        let terminal = settings.terminal;
        let cursor_style = terminal.cursor_style;
        let color_scheme = terminal.color_scheme;
        div()
            .id("preferences-terminal")
            .flex_1()
            .w_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .bg(theme.background)
            .child(page_heading(
                crate::locale::text("终端外观", "Terminal appearance"),
                crate::locale::text(
                    "终端运行参数和绘制偏好保存在 settings.toml。",
                    "Terminal runtime and rendering preferences are saved in settings.toml.",
                ),
                theme,
            ))
            .child(
                section_card(
                    crate::locale::text("字体与滚动", "Font and scrollback"),
                    theme,
                )
                .child(form_row(
                    crate::locale::text("字体", "Font family"),
                    crate::locale::text(
                        "填写系统已安装的字体族；终端缺字时仍由字体回退处理。",
                        "Use an installed font family. Missing glyphs still use font fallback.",
                    ),
                    Input::new(&self.settings_inputs.font_family)
                        .disabled(controls_locked)
                        .w_full()
                        .min_w(px(0.)),
                    theme,
                ))
                .child(form_row(
                    crate::locale::text("字号", "Font size"),
                    crate::locale::text("范围 10–24。", "Range: 10–24."),
                    Input::new(&self.settings_inputs.font_size)
                        .disabled(controls_locked)
                        .w(px(100.)),
                    theme,
                ))
                .child(form_row(
                    crate::locale::text("行高", "Line height"),
                    crate::locale::text("范围 1.0–1.8。", "Range: 1.0–1.8."),
                    Input::new(&self.settings_inputs.line_height)
                        .disabled(controls_locked)
                        .w(px(100.)),
                    theme,
                ))
                .child(form_row(
                    crate::locale::text("滚屏缓冲", "Scrollback"),
                    crate::locale::text("范围 1000–50000 行。", "Range: 1,000–50,000 lines."),
                    Input::new(&self.settings_inputs.scrollback)
                        .disabled(controls_locked)
                        .w(px(150.)),
                    theme,
                ))
                .child(
                    Switch::new("preferences-cursor-blink")
                        .checked(terminal.cursor_blink)
                        .disabled(controls_locked)
                        .label(crate::locale::text("光标闪烁", "Cursor blink"))
                        .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                            panel.edit_settings(
                                |settings| settings.terminal.cursor_blink = *enabled,
                                cx,
                            )
                        })),
                )
                .child(
                    Switch::new("preferences-selection-copy")
                        .checked(terminal.selection_copy)
                        .disabled(controls_locked)
                        .label(crate::locale::text(
                            "选中后复制到剪贴板",
                            "Copy selection to clipboard",
                        ))
                        .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                            panel.edit_settings(
                                |settings| settings.terminal.selection_copy = *enabled,
                                cx,
                            )
                        })),
                ),
            )
            .child(
                section_card(
                    crate::locale::text("光标与配色", "Cursor and color scheme"),
                    theme,
                )
                .child(
                    enum_selector(
                        crate::locale::text("光标样式", "Cursor style"),
                        Some(cursor_label(cursor_style)),
                        theme,
                    )
                    .child(enum_button(
                        "cursor-block",
                        crate::locale::text("块状", "Block"),
                        cursor_style == TerminalCursorStyle::Block,
                        controls_locked,
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.edit_settings(
                                |settings| {
                                    settings.terminal.cursor_style = TerminalCursorStyle::Block
                                },
                                cx,
                            )
                        }),
                    ))
                    .child(enum_button(
                        "cursor-bar",
                        crate::locale::text("竖线", "Bar"),
                        cursor_style == TerminalCursorStyle::Bar,
                        controls_locked,
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.edit_settings(
                                |settings| {
                                    settings.terminal.cursor_style = TerminalCursorStyle::Bar
                                },
                                cx,
                            )
                        }),
                    ))
                    .child(enum_button(
                        "cursor-underline",
                        crate::locale::text("下划线", "Underline"),
                        cursor_style == TerminalCursorStyle::Underline,
                        controls_locked,
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.edit_settings(
                                |settings| {
                                    settings.terminal.cursor_style = TerminalCursorStyle::Underline
                                },
                                cx,
                            )
                        }),
                    )),
                )
                .child(
                    enum_selector(
                        crate::locale::text("配色方案", "Color scheme"),
                        Some(color_scheme_label(color_scheme).into()),
                        theme,
                    )
                    .child(enum_button(
                        "scheme-kerminal",
                        "Kerminal",
                        color_scheme == TerminalColorScheme::Kerminal,
                        controls_locked,
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.edit_settings(
                                |settings| {
                                    settings.terminal.color_scheme = TerminalColorScheme::Kerminal
                                },
                                cx,
                            )
                        }),
                    ))
                    .child(enum_button(
                        "scheme-tokyo",
                        "Tokyo Night",
                        color_scheme == TerminalColorScheme::TokyoNight,
                        controls_locked,
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.edit_settings(
                                |settings| {
                                    settings.terminal.color_scheme = TerminalColorScheme::TokyoNight
                                },
                                cx,
                            )
                        }),
                    ))
                    .child(enum_button(
                        "scheme-solarized",
                        "Solarized",
                        color_scheme == TerminalColorScheme::Solarized,
                        controls_locked,
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.edit_settings(
                                |settings| {
                                    settings.terminal.color_scheme = TerminalColorScheme::Solarized
                                },
                                cx,
                            )
                        }),
                    ))
                    .child(enum_button(
                        "scheme-github",
                        "GitHub",
                        color_scheme == TerminalColorScheme::Github,
                        controls_locked,
                        cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.edit_settings(
                                |settings| {
                                    settings.terminal.color_scheme = TerminalColorScheme::Github
                                },
                                cx,
                            )
                        }),
                    )),
                ),
            )
            .child(self.render_terminal_behavior(theme, cx))
            .children(
                self.settings_issues
                    .iter()
                    .map(|issue| settings_issue_row(issue, theme)),
            )
            .children(
                self.settings_input_errors
                    .values()
                    .map(|message| issue_row(message, theme)),
            )
            .children(settings_error_banner(self.settings_error.as_deref(), theme))
    }

    /// 三个平台各占一列，Windows 主键和备用键同列；旧 Linux 回退值仅在用户编辑后显式保存。
    ///
    /// @author kongweiguang
    pub(super) fn render_shortcuts(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return self.render_settings_loading(theme);
        };
        let controls_locked = self.settings_controls_locked();
        let platform = keyboard_platform();
        let rows = settings.keybindings.iter().map(|binding| {
            let fields = self
                .shortcut_inputs
                .iter()
                .find(|inputs| inputs.action == binding.action);
            let related = self
                .settings_issues
                .iter()
                .filter(|issue| issue.actions.iter().any(|action| action == &binding.action));
            div()
                .w_full()
                .px_4()
                .py_3()
                .flex()
                .flex_col()
                .gap_2()
                .bg(theme.panel)
                .rounded_md()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .child(keybinding_label(&binding.action, &binding.label)),
                        )
                        .child(scope_badge(binding.scope, theme))
                        .child(if binding.editable {
                            crate::locale::text("", "")
                        } else {
                            crate::locale::text("内置", "Built-in")
                        }),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(binding.description.clone()),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_start()
                        .gap_3()
                        .child(shortcut_platform_cell(
                            crate::locale::text("Windows", "Windows"),
                            crate::locale::text("主键", "Primary"),
                            fields.map(|fields| fields.windows.clone()),
                            !binding.editable || controls_locked,
                            theme,
                            None,
                            Some((
                                crate::locale::text("备用", "Alternate"),
                                fields.map(|fields| fields.windows_alternate.clone()),
                            )),
                        ))
                        .child(shortcut_platform_cell(
                            crate::locale::text("Linux", "Linux"),
                            crate::locale::text("绑定", "Binding"),
                            fields.map(|fields| fields.linux.clone()),
                            !binding.editable || controls_locked,
                            theme,
                            Some(linux_binding_controls(
                                binding,
                                !binding.editable || controls_locked,
                                theme,
                                cx,
                            )),
                            None,
                        ))
                        .child(shortcut_platform_cell(
                            crate::locale::text("macOS", "macOS"),
                            crate::locale::text("绑定", "Binding"),
                            fields.map(|fields| fields.mac.clone()),
                            !binding.editable || controls_locked,
                            theme,
                            None,
                            None,
                        )),
                )
                .children(related.map(|issue| settings_issue_row(issue, theme)))
        });
        let platform_name = match platform {
            KeyboardPlatform::Windows => crate::locale::text("Windows", "Windows"),
            KeyboardPlatform::Linux => crate::locale::text("Linux", "Linux"),
            KeyboardPlatform::Mac => crate::locale::text("macOS", "macOS"),
        };
        div()
            .id("preferences-shortcuts")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .p_5()
            .flex()
            .flex_col()
            .gap_3()
            .bg(theme.background)
            .child(page_heading(
                crate::locale::text("快捷键", "Keyboard shortcuts"),
                crate::locale::text(
                    "校验当前平台并显示冲突。三平台分列；旧 Linux 配置沿用 Windows/通用键位，可用按钮显式禁用。Windows 备用键留空即禁用。组合键步骤以空格分隔。",
                    "Validates this platform and reports conflicts. Legacy Linux entries inherit the Windows/generic binding and can be explicitly disabled. A blank Windows alternate disables it. Separate chord steps with spaces.",
                ),
                theme,
            ))
            .child(
                div()
                    .w_full()
                    .text_size(px(12.))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        format!("当前检查平台：{platform_name}。组合键示例：Ctrl+K Ctrl+C。"),
                        format!("Validation uses {platform_name}. Chord example: Ctrl+K Ctrl+C."),
                    )),
            )
            .children(rows)
            .children(
                self.settings_input_errors
                    .values()
                    .map(|message| issue_row(message, theme)),
            )
            .children(settings_error_banner(self.settings_error.as_deref(), theme))
    }

    /// 设置暂未成功读取时显示错误或加载进度，不构造虚假的默认表单。
    fn render_settings_loading(&self, theme: ThemeTokens) -> gpui_kit::Stateful<gpui_kit::Div> {
        let message = self.settings_error.clone().unwrap_or_else(|| {
            if self.loading_settings {
                crate::locale::text("正在读取设置…", "Loading preferences…").to_string()
            } else {
                crate::locale::text("设置快照不可用。", "Settings snapshot is unavailable.")
                    .to_string()
            }
        });
        div()
            .id("preferences-settings-loading")
            .flex_1()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.muted)
            .child(message)
    }

    /// 异步载入设置后为各平台字段建独立输入，旧 Linux 回退值不写回直到用户明确编辑。
    ///
    /// @author kongweiguang
    pub(super) fn rebuild_shortcut_inputs(
        &mut self,
        settings: &SettingsSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shortcut_subscriptions.clear();
        self.shortcut_inputs.clear();
        for binding in &settings.keybindings {
            let windows_value = effective_binding(&binding.windows_binding, &binding.binding);
            let alternate_value = binding
                .windows_alternate_binding
                .clone()
                .unwrap_or_default();
            // 旧 Linux 的 None 保持为空编辑框，实际继承值由每次渲染计算后显示。
            let linux_value = binding.linux_binding.clone().unwrap_or_default();
            let mac_value = effective_binding(&binding.mac_binding, &binding.binding);
            let windows = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx)
                    .placeholder(crate::locale::text("主键组合", "Primary shortcut"))
                    .default_value(windows_value)
            });
            let windows_alternate = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx)
                    .placeholder(crate::locale::text(
                        "备用组合（可选）",
                        "Alternate shortcut (optional)",
                    ))
                    .default_value(alternate_value)
            });
            let linux = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx)
                    .placeholder(crate::locale::text("按键组合", "Key combination"))
                    .default_value(linux_value)
            });
            let mac = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx)
                    .placeholder(crate::locale::text("按键组合", "Key combination"))
                    .default_value(mac_value)
            });
            let windows_action = binding.action.clone();
            let windows_subscription = cx.subscribe(
                &windows,
                move |panel, input, event: &gpui_kit::component::input::InputEvent, cx| {
                    if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        panel.update_shortcut(
                            &windows_action,
                            ShortcutBindingField::Windows,
                            value,
                            cx,
                        );
                    }
                },
            );
            let alternate_action = binding.action.clone();
            let alternate_subscription = cx.subscribe(
                &windows_alternate,
                move |panel, input, event: &gpui_kit::component::input::InputEvent, cx| {
                    if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        panel.update_shortcut(
                            &alternate_action,
                            ShortcutBindingField::WindowsAlternate,
                            value,
                            cx,
                        );
                    }
                },
            );
            let linux_action = binding.action.clone();
            let linux_subscription = cx.subscribe(
                &linux,
                move |panel, input, event: &gpui_kit::component::input::InputEvent, cx| {
                    if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        panel.update_shortcut(
                            &linux_action,
                            ShortcutBindingField::Linux,
                            value,
                            cx,
                        );
                    }
                },
            );
            let mac_action = binding.action.clone();
            let mac_subscription = cx.subscribe(
                &mac,
                move |panel, input, event: &gpui_kit::component::input::InputEvent, cx| {
                    if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        panel.update_shortcut(&mac_action, ShortcutBindingField::Mac, value, cx);
                    }
                },
            );
            self.shortcut_inputs.push(ShortcutInputs {
                action: binding.action.clone(),
                windows,
                windows_alternate,
                linux,
                mac,
            });
            self.shortcut_subscriptions.push(windows_subscription);
            self.shortcut_subscriptions.push(alternate_subscription);
            self.shortcut_subscriptions.push(linux_subscription);
            self.shortcut_subscriptions.push(mac_subscription);
        }
    }
}

/// 输入修改后立即更新格式/冲突问题，让错误在保存之前显示。
impl PreferencesPanel {
    /// @author kongweiguang
    pub(super) fn refresh_settings_issues(&mut self) {
        if let Some(settings) = &self.settings {
            self.settings_issues = validate_settings(settings.value(), keyboard_platform());
        }
    }
}

/// 图片关闭时不构建专属控件或留下占位；实体与值仍由原 panel 持有，不在可见性分支清空草稿。
/// 只组合图片路径、选择、适配和图片透明度；窗口材质必须由调用方独立绘制，不受此开关限制。
///
/// @author kongweiguang
fn appearance_image_controls(
    enabled: bool,
    build: impl FnOnce(gpui_kit::Div) -> gpui_kit::Div,
) -> Option<gpui_kit::Div> {
    enabled.then(|| build(div().w_full().min_w(px(0.)).flex().flex_col().gap_4()))
}

/// basis 只决定一行能否容纳两张卡，min-width 为零让单列在窄正文中继续安全收缩。
/// 不设固定高度或持久化宽度，窗口夹紧、侧导航和 DPI 仍由既有布局分配。
/// @author kongweiguang
fn appearance_column(card: gpui_kit::Div) -> gpui_kit::Div {
    card.flex_1()
        .flex_basis(px(APPEARANCE_COLUMN_BASIS_PX))
        .min_w(px(0.))
}

/// 标签与说明放在控件上方，避免窄正文把说明压成逐字换行并给输入保留完整可用宽度。
///
/// @author kongweiguang
fn form_row(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    input: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    let description = description.into();
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(px(14.)).child(label.into()))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child(description),
                ),
        )
        .child(div().w_full().min_w(px(0.)).child(input))
}

/// 分组卡片提供统一间距和语义边界，并允许窄正文收缩而不被内容最小宽度撑开。
///
/// @author kongweiguang
fn section_card(title: impl Into<SharedString>, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .min_w(px(0.))
        .p_4()
        .flex()
        .flex_col()
        .gap_4()
        .bg(theme.panel)
        .rounded_md()
        .child(
            div()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title.into()),
        )
}

/// 让外部设置回读后的百分比驱动滑块，避免控件继续显示旧草稿值。
///
/// @author kongweiguang
fn sync_percentage_slider(
    slider: &Entity<SliderState>,
    value: u8,
    min: f32,
    max: f32,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    let current = slider.read(cx).value().end().round();
    let next = (value as f32).clamp(min, max);
    if current != next {
        slider.update(cx, |state, cx| state.set_value(next, window, cx));
    }
}

/// 用整数百分比和Kit滑块编辑透明度，精确到每一个配置值并保留读数。
///
/// @author kongweiguang
fn percentage_control(
    slider: &Entity<SliderState>,
    value: u8,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_3()
        .child(
            Slider::new(slider)
                .flex_1()
                .min_w(px(0.))
                .h(px(28.))
                .disabled(disabled),
        )
        .child(
            div()
                .w(px(56.))
                .flex_shrink_0()
                .text_size(px(14.))
                .text_color(theme.foreground)
                .text_right()
                .child(format!("{value}%")),
        )
}

/// 设置页主标题区提供清晰层级，避免控件本身承担说明文本职责。
fn page_heading(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    let title = title.into();
    let description = description.into();
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(20.))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(title),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(description),
        )
}

/// 枚举选项保持按钮自身宽度，让父级换行而不是压缩文字；当前选择仍用 primary 状态表示。
///
/// @author kongweiguang
fn enum_button(
    id: &'static str,
    label: impl Into<SharedString>,
    selected: bool,
    disabled: bool,
    listener: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> Button {
    let button = Button::new(id)
        .compact()
        .flex_shrink_0()
        .label(label)
        .disabled(disabled)
        .on_click(listener);
    if selected { button.primary() } else { button }
}

/// 合并主题/密度选择卡的状态与 Kit app 上下文，避免八个平行参数在两类卡片间错位。
///
/// @author kongweiguang
struct ChoiceCardStyle {
    selected: bool,
    disabled: bool,
    theme: ThemeTokens,
}

/// 三态主题卡用弱强调选中色；Kit 自定义状态需由 App 构造并统一承载 hover/press 颜色。
///
/// 这样既保留 Kit 的可访问选择状态，也避免依赖当前版本 Button 不提供的 active 样式扩展。
///
/// @author kongweiguang
fn theme_choice_card(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    style: ChoiceCardStyle,
    listener: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
    cx: &gpui_kit::App,
) -> Button {
    let ChoiceCardStyle {
        selected,
        disabled,
        theme,
    } = style;
    let label = label.into();
    let fill = if selected {
        theme.accent.opacity(0.12)
    } else {
        theme.secondary
    };
    let border = if selected {
        theme.accent.opacity(0.62)
    } else {
        theme.border
    };
    let hover = if selected {
        theme.accent.opacity(0.18)
    } else {
        theme.hover
    };
    let active = if selected {
        theme.accent.opacity(0.24)
    } else {
        theme.secondary
    };
    Button::new(id)
        .flex_1()
        .min_w(px(0.))
        .h(px(THEME_CHOICE_HEIGHT_PX))
        .rounded(theme.radius)
        .outline()
        .border_1()
        .border_color(border)
        .selected(selected)
        .toggled(selected)
        .accessibility_label(crate::locale::text(
            format!("主题：{label}"),
            format!("Theme: {label}"),
        ))
        .tab_stop(true)
        .tab_index(0)
        .focus_ring(true)
        .disabled(disabled)
        .on_click(listener)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(fill)
                .foreground(theme.foreground)
                .hover(hover)
                .active(active),
        )
        .child(
            div()
                .w_full()
                .h_full()
                .px_2()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(icon).size(px(20.)).text_color(if selected {
                    theme.accent
                } else {
                    theme.muted
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme.foreground)
                                .child(label),
                        )
                        .when(selected, |row| {
                            row.child(
                                Icon::new(IconName::CircleCheck)
                                    .small()
                                    .text_color(theme.accent)
                                    .flex_shrink_0(),
                            )
                        }),
                ),
        )
}

/// 紧凑密度分段保留逐项焦点与可访问名称，同时让选中态和保存值始终指向同一选项。
///
/// @author kongweiguang
fn density_choice_segment(
    choice: DensityChoiceSpec,
    style: ChoiceCardStyle,
    listener: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
    cx: &gpui_kit::App,
) -> Button {
    let ChoiceCardStyle {
        selected,
        disabled,
        theme,
    } = style;
    let label = crate::locale::text(choice.label_zh, choice.label_en);
    let fill = if selected {
        theme.accent.opacity(0.12)
    } else {
        theme.secondary
    };
    let border = if selected {
        theme.accent.opacity(0.62)
    } else {
        theme.border
    };
    let hover = if selected {
        theme.accent.opacity(0.18)
    } else {
        theme.hover
    };
    let active = if selected {
        theme.accent.opacity(0.24)
    } else {
        theme.secondary
    };
    Button::new(choice.id)
        .flex_1()
        .min_w(px(DENSITY_SEGMENT_MIN_WIDTH_PX))
        .h(px(DENSITY_SEGMENT_HEIGHT_PX))
        .rounded(theme.radius_sm)
        .outline()
        .border_1()
        .border_color(border)
        .selected(selected)
        .toggled(selected)
        .accessibility_label(crate::locale::text(
            format!("界面密度：{label}"),
            format!("Interface density: {label}"),
        ))
        .tab_stop(true)
        .tab_index(0)
        .focus_ring(true)
        .disabled(disabled)
        .on_click(listener)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(fill)
                .foreground(theme.foreground)
                .hover(hover)
                .active(active),
        )
        .child(
            div()
                .w_full()
                .h_full()
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div().flex().items_center().justify_center().child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(label),
                    ),
                )
                .when(selected, |row| {
                    row.child(
                        Icon::new(IconName::CircleCheck)
                            .small()
                            .text_color(theme.accent)
                            .flex_shrink_0(),
                    )
                }),
        )
}

/// 标签和可选的当前值占独立整宽说明区，后续选项作为可换行按钮排列。
///
/// @author kongweiguang
fn enum_selector(
    label: impl Into<SharedString>,
    current: Option<SharedString>,
    theme: ThemeTokens,
) -> gpui_kit::Div {
    let mut label_group = div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_1()
        .child(div().w_full().text_size(px(14.)).child(label.into()));
    if let Some(current) = current {
        label_group = label_group.child(
            div()
                .w_full()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(crate::locale::text(
                    format!("当前：{current}"),
                    format!("Current: {current}"),
                )),
        );
    }
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(label_group)
}

/// 校验项始终用项目语义错误色显示，与浅色、深色主题令牌同步。
fn issue_row(message: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(theme.error)
        .child(message.to_owned())
}

/// 兼容性未知 action 以提示色呈现，其余会阻止保存的问题继续使用错误色。
fn settings_issue_row(issue: &super::SettingsIssue, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(if issue.blocks_save() {
            theme.error
        } else {
            theme.muted
        })
        .child(issue.message.clone())
}

/// 快捷键作用域 badge 与配置类型保持同源，不从 label 文本推断行为。
///
/// @author kongweiguang
fn scope_badge(scope: KeybindingScope, theme: ThemeTokens) -> impl IntoElement {
    let label = match scope {
        KeybindingScope::Global => crate::locale::text("全局", "Global"),
        KeybindingScope::Terminal => crate::locale::text("终端", "Terminal"),
        KeybindingScope::Workspace => crate::locale::text("工作区", "Workspace"),
    };
    div()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(theme.sidebar)
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(label)
}

/// 已知动作使用当前界面语义，标签切换覆盖全部类型；旧 action ID 和未知扩展的用户标签继续保留。
pub(super) fn keybinding_label(action: &str, fallback: &str) -> SharedString {
    let (zh_cn, en_us) = match action {
        "settings.open" => ("打开设置", "Open settings"),
        "settings.keybindings" => ("查看快捷键与动作", "View shortcuts and actions"),
        "terminal.focus" => ("回到终端", "Focus terminal"),
        "terminal.paste" => ("粘贴到终端", "Paste into terminal"),
        "terminal.newTab" => ("新建本地终端", "New local terminal"),
        "terminal.closeTab" => ("关闭当前终端标签", "Close current terminal tab"),
        "terminal.closePane" => ("关闭当前分屏", "Close current pane"),
        "terminal.splitHorizontal" => ("左右分屏", "Split left and right"),
        "terminal.splitVertical" => ("上下分屏", "Split top and bottom"),
        "terminal.previousTab" => ("上一个标签", "Previous tab"),
        "terminal.nextTab" => ("下一个标签", "Next tab"),
        "tool.agentLauncher" => ("打开 Agent Launcher", "Open Agent Launcher"),
        "tool.system" => ("打开系统信息", "Open system information"),
        "tool.sftp" => ("打开 SFTP", "Open SFTP"),
        "tool.ports" => ("打开端口转发", "Open port forwarding"),
        "tool.snippets" => ("打开脚本片段", "Open command snippets"),
        "tool.logs" => ("打开日志", "Open logs"),
        _ => return fallback.to_owned().into(),
    };
    crate::locale::text(zh_cn, en_us)
}

/// 回退通用键位用于显示和初始编辑，但仅在用户保存时写入对应平台字段。
fn effective_binding(platform: &str, generic: &str) -> String {
    if platform.is_empty() {
        generic.to_owned()
    } else {
        platform.to_owned()
    }
}

/// 背景适配方式文案描述实际绘制行为。
fn fit_label(value: BackgroundFit) -> SharedString {
    match value {
        BackgroundFit::Cover => crate::locale::text("填充", "Cover"),
        BackgroundFit::Contain => crate::locale::text("完整显示", "Contain"),
        BackgroundFit::Tile => crate::locale::text("平铺", "Tile"),
    }
}
/// 终端光标形态文案对应 Alacritty terminal 可用样式。
fn cursor_label(value: TerminalCursorStyle) -> SharedString {
    match value {
        TerminalCursorStyle::Block => crate::locale::text("块状", "Block"),
        TerminalCursorStyle::Bar => crate::locale::text("竖线", "Bar"),
        TerminalCursorStyle::Underline => crate::locale::text("下划线", "Underline"),
    }
}
/// 色盘名称使用用户可识别品牌和社区主题名。
fn color_scheme_label(value: TerminalColorScheme) -> &'static str {
    match value {
        TerminalColorScheme::Kerminal => "Kerminal",
        TerminalColorScheme::TokyoNight => "Tokyo Night",
        TerminalColorScheme::Solarized => "Solarized",
        TerminalColorScheme::Github => "GitHub",
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/preferences_appearance.rs"]
mod appearance_tests;
