//! 主题、界面、终端外观和快捷键的原生控件。
//!
//! @author kongweiguang

use crate::theme::ThemeTokens;
use gpui_kit::{
    ClickEvent, Context, Entity, IntoElement, SharedString, StatefulInteractiveElement, Window,
    assets::IconName,
    base::Selectable,
    component::{
        Disableable, FocusableExt as _, Icon, Sizable as _,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        input::Input,
        select::Select,
        slider::{Slider, SliderState},
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::SettingsSnapshot;

use super::{
    BackgroundFit, InterfaceDensity, ThemeMode,
    panel::{PreferencesPanel, ShortcutBindingField, ShortcutInputs, keyboard_platform},
    render::settings_error_banner,
    validate_settings,
};

const THEME_CHOICE_HEIGHT_PX: f32 = 80.0;
const DENSITY_CHOICE_MIN_HEIGHT_PX: f32 = 64.0;
const BACKGROUND_FIT_CHOICE_MIN_HEIGHT_PX: f32 = 96.0;

/// 表单字段覆盖 Kit 默认工作台表面，保留原输入实体、校验与焦点行为。
/// 使用透明字段表面才能在浅深色的 main 卡片背景上保持同样层次。
///
/// @author kongweiguang
pub(super) fn settings_input(
    input: &Entity<gpui_kit::component::input::InputState>,
    theme: ThemeTokens,
) -> Input {
    Input::new(input)
        .bg(theme.field_surface())
        .border_color(theme.strong_border())
        .rounded(theme.control_radius)
        .font_weight(gpui_kit::FontWeight::NORMAL)
}

/// main 的安静按钮只使用中性 hover；默认 Kit Ghost 的动作选中色会把折叠项误画成选中页。
///
/// @author kongweiguang
pub(super) fn settings_ghost_variant(
    theme: ThemeTokens,
    cx: &gpui_kit::App,
) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .foreground(theme.settings_choice_foreground(false))
        .hover(theme.surface_hover())
        .active(theme.surface_hover())
}

/// 将选择卡文案与布局要求作为一个选项单元，避免标签、说明和尺寸错位。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
struct PreferenceChoiceCardSpec {
    id: &'static str,
    label_zh: &'static str,
    label_en: &'static str,
    description_zh: &'static str,
    description_en: &'static str,
    min_height: f32,
    full_width: bool,
}

/// 同一组密度选项共用标签、值与稳定 ID，避免分段按钮和保存回调各自维护不同顺序。
/// @author kongweiguang
#[derive(Clone, Copy)]
struct DensityChoiceSpec {
    id: &'static str,
    value: InterfaceDensity,
    label_zh: &'static str,
    label_en: &'static str,
    description_zh: &'static str,
    description_en: &'static str,
}

const DENSITY_CHOICES: [DensityChoiceSpec; 3] = [
    DensityChoiceSpec {
        id: "density-compact",
        value: InterfaceDensity::Compact,
        label_zh: "紧凑",
        label_en: "Compact",
        description_zh: "更紧凑，适合小屏。",
        description_en: "More compact for smaller screens.",
    },
    DensityChoiceSpec {
        id: "density-comfortable",
        value: InterfaceDensity::Comfortable,
        label_zh: "舒适",
        label_en: "Comfortable",
        description_zh: "默认留白。",
        description_en: "Default spacing.",
    },
    DensityChoiceSpec {
        id: "density-spacious",
        value: InterfaceDensity::Spacious,
        label_zh: "宽松",
        label_en: "Spacious",
        description_zh: "留白更多。",
        description_en: "More spacing.",
    },
];

/// 背景适配选项与 main 的标签、说明和稳定身份保持同一顺序。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
struct BackgroundFitChoiceSpec {
    id: &'static str,
    value: BackgroundFit,
    label_zh: &'static str,
    label_en: &'static str,
    description_zh: &'static str,
    description_en: &'static str,
}

const BACKGROUND_FIT_CHOICES: [BackgroundFitChoiceSpec; 3] = [
    BackgroundFitChoiceSpec {
        id: "fit-cover",
        value: BackgroundFit::Cover,
        label_zh: "填充画布",
        label_en: "Fill canvas",
        description_zh: "铺满工作台。",
        description_en: "Fill the workbench.",
    },
    BackgroundFitChoiceSpec {
        id: "fit-contain",
        value: BackgroundFit::Contain,
        label_zh: "完整显示",
        label_en: "Fit image",
        description_zh: "完整显示，避免裁切。",
        description_en: "Show the whole image without cropping.",
    },
    BackgroundFitChoiceSpec {
        id: "fit-tile",
        value: BackgroundFit::Tile,
        label_zh: "平铺纹理",
        label_en: "Tile texture",
        description_zh: "重复平铺。",
        description_en: "Repeat the image as a tile.",
    },
];

impl PreferencesPanel {
    /// 加载、只读恢复、冲突与未知写入结果锁定表单；保存中仍允许编辑以保留连续输入体验。
    pub(super) fn settings_controls_locked(&self) -> bool {
        self.loading_settings
            || self.recovery.mode
                == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery
            || self.settings.as_ref().is_some_and(|draft| {
                matches!(
                    draft.status(),
                    super::DraftStatus::ExternalConflict | super::DraftStatus::OutcomeUnknown
                )
            })
    }

    /// 按 main 的 viewport xl 断点切换外观卡片列数，窄窗纵向使用自然高度以保留滚动内容。
    /// 背景开关仍只影响工作台绘制，所有修改继续进入同一 typed 草稿和既有保存门禁。
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
        let density = settings.interface_density;
        let background_enabled = settings.background_enabled;
        let background_fit = settings.background_fit;
        let viewport_width = window.viewport_size().width.as_f32();
        let medium_layout = viewport_width >= 768.0;
        let wide_layout = viewport_width >= 1280.0;
        let issue_rows = self
            .settings_issues
            .iter()
            .map(|issue| settings_issue_row(issue, theme));
        let input_errors = self
            .settings_input_errors
            .values()
            .map(|message| issue_row(message, theme));
        let background_fit_cards = BACKGROUND_FIT_CHOICES
            .into_iter()
            .map(|choice| {
                let card = PreferenceChoiceCardSpec {
                    id: choice.id,
                    label_zh: choice.label_zh,
                    label_en: choice.label_en,
                    description_zh: choice.description_zh,
                    description_en: choice.description_en,
                    min_height: BACKGROUND_FIT_CHOICE_MIN_HEIGHT_PX,
                    full_width: false,
                };
                preference_choice_card(
                    card,
                    background_fit == choice.value,
                    controls_locked,
                    theme,
                    cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.edit_settings(|settings| settings.background_fit = choice.value, cx)
                    }),
                    cx,
                )
                .into_any_element()
            })
            .collect();

        let mut body = div()
            .id("preferences-appearance")
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
                    .min_w(px(0.))
                    .flex()
                    .when(wide_layout, |layout| {
                        layout.flex_row().items_stretch()
                    })
                    .when(!wide_layout, |layout| layout.flex_col())
                    .gap_4()
                    .child(
                        section_panel(theme)
                    .when(wide_layout, |card| appearance_column(card, 0.92))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(IconName::Languages).small().text_color(theme.muted))
                            .child(
                                div()
                                    .text_size(theme.text_body)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(crate::locale::text("基础外观", "Basic appearance")),
                            ),
                    )
                    .child(form_row(
                        crate::locale::text("界面语言", "Interface language"),
                        Select::new(&self.interface_language_select)
                            .bg(theme.field_surface())
                            .border_color(theme.strong_border())
                            .rounded(theme.control_radius)
                            .id("settings-interface-language-select")
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
                        crate::locale::text("应用外观", "App appearance"),
                        div()
                            .w_full()
                            .flex()
                                    .gap_2()
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
                    .child(percentage_control(
                        crate::locale::text("界面透明度", "Interface opacity"),
                        &self.window_opacity_slider,
                        settings.window_opacity,
                        controls_locked,
                        theme,
                    ))
                    .child(form_row(
                        crate::locale::text("界面密度", "Interface density"),
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(DENSITY_CHOICES.into_iter().map(|choice| {
                                let card = PreferenceChoiceCardSpec {
                                    id: choice.id,
                                    label_zh: choice.label_zh,
                                    label_en: choice.label_en,
                                    description_zh: choice.description_zh,
                                    description_en: choice.description_en,
                                    min_height: DENSITY_CHOICE_MIN_HEIGHT_PX,
                                    full_width: true,
                                };
                                preference_choice_card(
                                    card,
                                    density == choice.value,
                                    controls_locked,
                                    theme,
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
                    )),
            )
                    .child(
                        section_panel(theme)
                    .when(wide_layout, |card| appearance_column(card, 1.08))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_start()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(Icon::new(IconName::Image).size(px(16.)).text_color(theme.muted))
                                            .child(
                                                div()
                                                    .text_size(theme.text_body)
                                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                    .child(crate::locale::text(
                                                        "主页面背景",
                                                        "Workbench background",
                                                    )),
                                            ),
                                    )
                                    .child(
                                                div()
                                                    .text_size(theme.text_xs)
                                                    .line_height(px(20.))
                                                    .text_color(theme.muted)
                                                    .child(crate::locale::text(
                                                        "在工作台底层显示本地图片。",
                                                        "Display a local image behind the workbench.",
                                                    )),
                                    ),
                            )
                            .child(
                                Switch::new("preferences-background-enabled")
                                    .track_size(px(40.), px(24.))
                                    .checked(background_enabled)
                                    .disabled(controls_locked)
                                    .accessibility_label(crate::locale::text(
                                        "启用主页面背景",
                                        "Enable workbench background",
                                    ))
                                    .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                        panel.edit_settings(
                                            |settings| settings.background_enabled = *enabled,
                                            cx,
                                        )
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id("background-opacity-control")
                            .child(percentage_control(
                                crate::locale::text("背景透明度", "Background opacity"),
                                &self.background_opacity_slider,
                                settings.background_opacity,
                                controls_locked,
                                theme,
                            )),
                    )
                    .child(
                        div()
                            .id("background-fit-controls")
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(theme.settings_medium_font_weight())
                                    .text_color(theme.muted)
                                    .child(crate::locale::text("背景铺放", "Image fit")),
                            )
                            .child(settings_grid_rows(
                                background_fit_cards,
                                if medium_layout {
                                    &[1.0, 1.0, 1.0]
                                } else {
                                    &[1.0]
                                },
                                SettingsGridGap::Compact,
                                false,
                            )),
                    )
                    .child(
                        div()
                            .id("background-path-control")
                            .child(form_row(
                                crate::locale::text("背景图路径", "Background image path"),
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .flex()
                                    .when(medium_layout, |row| {
                                        row.flex_row().items_end()
                                    })
                                    .when(!medium_layout, |column| column.flex_col())
                                    .gap_2()
                                    .child(
                                        gpui_kit::Styled::h(settings_input(&self.settings_inputs.background_path, theme), px(36.))
                                            .disabled(controls_locked)
                                            .w_full()
                                            .min_w(px(0.)),
                                    )
                                    .child(
                                        Button::new("preferences-choose-background")
                                            .compact()
                                            .flex_shrink_0()
                                            .h(px(36.))
                                            .label(crate::locale::text("浏览", "Browse"))
                                            .icon(IconName::Image)
                                            .disabled(controls_locked)
                                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                                panel.choose_background_image(cx)
                                            })),
                                    ),
                                theme,
                            )),
                    ),
                    ),
            );
        if let Some(error) = settings_error_banner(self.settings_error.as_deref(), theme) {
            body = body.child(error);
        }
        body.children(issue_rows)
            .children(input_errors)
            .child(self.render_save_notice(theme))
    }

    /// 设置暂未成功读取时显示错误或加载进度，不构造虚假的默认表单。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn render_settings_loading(
        &self,
        theme: ThemeTokens,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
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

/// xl 双栏使用包含内边距的百分比宽度，再由 flex 按比例扣除列间距，复现 main 的 0.92fr/1.08fr。
/// 不能使用零 basis 的 grow，因为它会在分配比例之后另加内边距，令两列偏离 CSS grid 的外框比例。
/// @author kongweiguang
fn appearance_column(card: gpui_kit::Div, grow: f32) -> gpui_kit::Div {
    card.w(gpui_kit::relative(grow / 2.0)).min_w(px(0.))
}

/// 已由 viewport 断点选定列数后再顺序分行，避免子控件最小宽度决定列数并裁切内容。
/// @author kongweiguang
#[derive(Clone, Copy)]
pub(super) enum SettingsGridGap {
    Compact,
    Standard,
}

/// 顺序分行并填充空槽以保留固定列宽；末行不扩展单元格，也不改变阅读顺序。
/// @author kongweiguang
pub(super) fn settings_grid_rows(
    items: Vec<gpui_kit::AnyElement>,
    column_weights: &[f32],
    gap: SettingsGridGap,
    align_items_end: bool,
) -> gpui_kit::Div {
    // Preserve CSS grid's column widths on the final row instead of stretching its remaining item.
    let columns = column_weights.len().max(1);
    let mut items = items.into_iter();
    let mut rows = Vec::new();

    loop {
        let mut controls = items.by_ref().take(columns).collect::<Vec<_>>();
        if controls.is_empty() {
            break;
        }
        while controls.len() < columns {
            controls.push(div().into_any_element());
        }

        let cells = controls.into_iter().enumerate().map(|(column, control)| {
            let weight = column_weights
                .get(column)
                .copied()
                .filter(|weight| weight.is_finite() && *weight > 0.0)
                .unwrap_or(1.0);
            div()
                .flex()
                .flex_grow(weight)
                .flex_basis(px(0.))
                .min_w(px(0.))
                .child(control)
        });
        let mut row = div().w_full().flex().flex_row();
        row = match gap {
            SettingsGridGap::Compact => row.gap_2(),
            SettingsGridGap::Standard => row.gap_3(),
        };
        if align_items_end {
            row = row.items_end();
        }
        rows.push(row.children(cells));
    }

    let mut grid = div().w_full().flex().flex_col();
    grid = match gap {
        SettingsGridGap::Compact => grid.gap_2(),
        SettingsGridGap::Standard => grid.gap_3(),
    };
    grid.children(rows)
}

/// 字段统一按 main 的 12px 次要标签和紧邻控件排列，避免工具栏标签令牌提高表单的视觉权重。
///
/// @author kongweiguang
fn form_row(
    label: impl Into<SharedString>,
    input: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
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
                .text_size(px(12.))
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.muted)
                .child(label.into()),
        )
        .child(div().w_full().min_w(px(0.)).child(input))
}

/// 为带图标、辅助说明或开关的卡片标题复用同一边界，避免空标题占用垂直空间。
///
/// @author kongweiguang
fn section_panel(theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .min_w(px(0.))
        .p_4()
        .flex()
        .flex_col()
        .gap_4()
        .bg(theme.panel)
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius_panel)
}

/// Kit's Custom variant retains 20% of its normal color; compensate only its input alpha to match main's muted fill.
/// Hover and active colors remain untouched so pointer feedback keeps the component's native state transitions.
///
/// @author kongweiguang
fn settings_choice_variant_fill(fill: gpui_kit::Hsla) -> gpui_kit::Hsla {
    fill.alpha((fill.a / 0.2).min(1.0))
}

/// 将百分比标签和当前值按 main 的次要/正文层级并排呈现，保留 Kit 滑块的键鼠行为。
///
/// @author kongweiguang
fn percentage_control(
    label: impl Into<SharedString>,
    slider: &Entity<SliderState>,
    value: u8,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(theme.settings_medium_font_weight())
                        .text_color(theme.muted)
                        .child(label.into()),
                )
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(theme.foreground)
                        .child(format!("{value}%")),
                ),
        )
        .child(
            Slider::new(slider)
                .w_full()
                .min_w(px(0.))
                .h(px(16.))
                .bg(theme.tunnel_action)
                .text_color(theme.tunnel_action)
                .disabled(disabled),
        )
}

/// 合并主题选择卡的状态与 Kit app 上下文，避免并行参数在三态卡片间错位。
///
/// @author kongweiguang
struct ChoiceCardStyle {
    selected: bool,
    disabled: bool,
    theme: ThemeTokens,
}

/// 三态主题卡以底色表达选择，沿用 main 的控件圆角、图标与标题两层，不额外增加选中图标。
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
        theme.surface_selected()
    } else {
        theme.muted_surface()
    };
    let border = if selected {
        theme.tunnel_action.opacity(0.35)
    } else {
        theme.border.opacity(0.8)
    };
    let hover = if selected {
        theme.surface_selected()
    } else {
        theme.surface_hover()
    };
    let active = if selected {
        theme.surface_selected()
    } else {
        theme.muted_surface()
    };
    Button::new(id)
        .flex_1()
        .min_w(px(0.))
        .min_h(px(THEME_CHOICE_HEIGHT_PX))
        .rounded(theme.control_radius)
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
                .color(settings_choice_variant_fill(fill))
                .foreground(theme.settings_choice_foreground(selected))
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
                .child(
                    Icon::new(icon)
                        .size(px(16.))
                        .text_color(theme.settings_choice_foreground(selected)),
                )
                .child(
                    div().flex().items_center().justify_center().gap_2().child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui_kit::FontWeight::NORMAL)
                            .text_color(theme.settings_choice_foreground(selected))
                            .child(label),
                    ),
                ),
        )
}

/// 密度与背景铺放复用受控状态及 main 的 12px/10px 内边距；铺放卡允许窄列换行。
///
/// @author kongweiguang
fn preference_choice_card(
    spec: PreferenceChoiceCardSpec,
    selected: bool,
    disabled: bool,
    theme: ThemeTokens,
    listener: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
    cx: &gpui_kit::App,
) -> Button {
    let PreferenceChoiceCardSpec {
        id,
        label_zh,
        label_en,
        description_zh,
        description_en,
        min_height,
        full_width,
    } = spec;
    let label = crate::locale::text(label_zh, label_en);
    let description = crate::locale::text(description_zh, description_en);
    let fill = if selected {
        theme.surface_selected()
    } else {
        theme.muted_surface()
    };
    let border = if selected {
        theme.tunnel_action.opacity(0.35)
    } else {
        theme.border.opacity(0.8)
    };
    let hover = if selected {
        theme.surface_selected()
    } else {
        theme.surface_hover()
    };
    let active = if selected {
        theme.surface_selected()
    } else {
        theme.muted_surface()
    };
    Button::new(id)
        .when(full_width, |button| button.w_full())
        .when(!full_width, |button| {
            button.flex_1().flex_basis(px(0.)).min_w(px(0.))
        })
        .min_h(px(min_height))
        .px_3()
        .py(px(10.))
        .rounded(theme.control_radius)
        .outline()
        .border_1()
        .border_color(border)
        .selected(selected)
        .toggled(selected)
        .accessibility_label(crate::locale::text(
            format!("{label}：{description}"),
            format!("{label}: {description}"),
        ))
        .tab_stop(true)
        .tab_index(0)
        .focus_ring(true)
        .disabled(disabled)
        .on_click(listener)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(settings_choice_variant_fill(fill))
                .foreground(theme.settings_choice_foreground(selected))
                .hover(hover)
                .active(active),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .text_size(theme.text_sm)
                                .line_height(px(20.))
                                .font_weight(theme.settings_medium_font_weight())
                                .text_color(theme.settings_choice_foreground(selected))
                                .child(label),
                        )
                        .when(selected && full_width, |row| {
                            row.child(
                                Icon::new(IconName::Check)
                                    .small()
                                    .text_color(theme.accent)
                                    .flex_shrink_0(),
                            )
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(theme.text_xs)
                        .line_height(px(20.))
                        .text_color(theme.muted)
                        .child(description),
                ),
        )
}

/// 校验项使用语义错误色，在浅深主题中保留同等可读性。
///
/// @author kongweiguang
pub(in crate::features::preferences) fn issue_row(
    message: &str,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(theme.error)
        .child(message.to_owned())
}

/// 兼容性未知 action 以提示色呈现，其余会阻止保存的问题继续使用错误色。
pub(in crate::features::preferences) fn settings_issue_row(
    issue: &super::SettingsIssue,
    theme: ThemeTokens,
) -> impl IntoElement {
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

/// 回退通用键位用于显示和初始编辑，但仅在用户保存时写入对应平台字段。
fn effective_binding(platform: &str, generic: &str) -> String {
    if platform.is_empty() {
        generic.to_owned()
    } else {
        platform.to_owned()
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/preferences_appearance.rs"]
mod appearance_tests;
