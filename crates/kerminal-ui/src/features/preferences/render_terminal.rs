//! GPUI 终端设置页：保留 main 的主题、字体预览、交互与光标选项。
//!
//! @author kongweiguang

use gpui_kit::{
    Animation, AnimationExt as _, AppContext as _, ClickEvent, Context, Entity, IntoElement,
    SharedString, Subscription, Window,
    assets::IconName,
    component::{
        Disableable as _, FocusableExt as _, IndexPath, Selectable as _, Sizable as _,
        button::{Button, ButtonCustomVariant, ButtonVariants as _},
        input::{InputState, NumberInput, NumberStep},
        menu::{DropdownMenu as _, PopupMenuItem},
        searchable_list::SearchableVec,
        select::{Select, SelectEvent, SelectState},
        switch::Switch,
    },
    div,
    prelude::*,
    px, rems,
};
use kerminal_application::preferences::{
    SettingsSnapshot, TerminalColorScheme, TerminalCursorStyle, TerminalFontWeight,
    TerminalRightClickBehavior, ThemeMode,
};
use std::time::Duration;

use crate::{
    terminal_metrics::{TerminalMetrics, font_weight},
    theme::ThemeTokens,
};

use super::{
    DraftStatus, SettingsIssueKind,
    panel::{PreferencesPanel, SettingsInputs, SettingsSuccessStatus},
};

#[derive(Clone, Copy)]
struct SchemeOption {
    id: &'static str,
    value: TerminalColorScheme,
    label: &'static str,
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
    },
    SchemeOption {
        id: "tokyo-night",
        value: TerminalColorScheme::TokyoNight,
        label: "Tokyo Night",
    },
    SchemeOption {
        id: "solarized",
        value: TerminalColorScheme::Solarized,
        label: "Solarized",
    },
    SchemeOption {
        id: "github",
        value: TerminalColorScheme::Github,
        label: "GitHub",
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

/// 随设置页渲染周期保留选择器与订阅；切换设置页后自然释放。
struct SystemFontPicker {
    installed: Vec<String>,
    select: Entity<SelectState<SearchableVec<String>>>,
    _subscriptions: Vec<Subscription>,
}

impl SystemFontPicker {
    /// 只在首次显示时枚举系统字体，并在配置读回变化时同步显示实际回退族。
    ///
    /// @author kongweiguang
    fn new(
        owner: Entity<PreferencesPanel>,
        terminal: &kerminal_application::preferences::TerminalSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let installed = window.text_system().all_font_names();
        let resolved = TerminalMetrics::from_settings(terminal, &installed)
            .font_family
            .to_string();
        let mut options = installed.clone();
        if !options.iter().any(|option| option == &resolved) {
            options.push(resolved.clone());
        }
        let selected = options
            .iter()
            .position(|option| option == &resolved)
            .map(IndexPath::new);
        let select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(options), selected, window, cx).searchable(true)
        });
        let weak_owner = owner.downgrade();
        let selection = cx.subscribe(
            &select,
            move |_, _, event: &SelectEvent<SearchableVec<String>>, cx| {
                let SelectEvent::Confirm(Some(value)) = event else {
                    return;
                };
                let value = value.clone();
                let _ = weak_owner.update(cx, |panel, cx| {
                    panel.edit_settings(|settings| settings.terminal.font_family = value, cx);
                });
            },
        );
        let observation = cx.observe_in(&owner, window, |picker, owner, window, cx| {
            let Some(terminal) = owner
                .read(cx)
                .settings
                .as_ref()
                .map(|settings| settings.value().terminal.clone())
            else {
                return;
            };
            let resolved = TerminalMetrics::from_settings(&terminal, &picker.installed)
                .font_family
                .to_string();
            if picker.select.read(cx).selected_value() != Some(&resolved) {
                picker.select.update(cx, |select, cx| {
                    select.set_selected_value(&resolved, window, cx)
                });
            }
        });
        Self {
            installed,
            select,
            _subscriptions: vec![selection, observation],
        }
    }
}

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

    /// 围绕同一终端预览组织外观、光标和操作；单列分组避免无关区域互相撑高，所有控件仍写入唯一自动保存草稿。
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
        // 设置弹框有最大宽度和独立侧栏；按实际可用宽度与 rem 比例决定列数，避免大窗口或放大文字误判。
        let measured_width = self.content_scroll.bounds().size.width.as_f32();
        let content_width = if measured_width > 0.0 {
            measured_width
        } else {
            viewport_width.min(1152.0) - 280.0
        };
        let medium_layout = content_width >= window.rem_size().as_f32() * 28.0;
        let wide_layout = content_width >= window.rem_size().as_f32() * 40.0;
        let is_dark = match settings.theme_mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => matches!(
                window.appearance(),
                gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
            ),
        };
        let owner = cx.entity();
        let initial_terminal = terminal.clone();
        let font_picker =
            window.use_keyed_state("preferences-terminal-font-picker", cx, move |window, cx| {
                SystemFontPicker::new(owner, &initial_terminal, window, cx)
            });
        let picker = font_picker.read(cx);
        let resolved_font_family =
            TerminalMetrics::from_settings(&terminal, &picker.installed).font_family;
        let font_select = picker.select.clone();

        let font_weight_options = vec![
            (
                TerminalFontWeight::Normal,
                crate::locale::text("常规", "Normal"),
            ),
            (
                TerminalFontWeight::Medium,
                crate::locale::text("中等", "Medium"),
            ),
            (
                TerminalFontWeight::Bold,
                crate::locale::text("加粗", "Bold"),
            ),
        ];
        let right_click_options = vec![
            (
                TerminalRightClickBehavior::None,
                crate::locale::text("不执行", "None"),
            ),
            (
                TerminalRightClickBehavior::Paste,
                crate::locale::text("粘贴", "Paste"),
            ),
            (
                TerminalRightClickBehavior::Menu,
                crate::locale::text("菜单", "Menu"),
            ),
        ];

        let font_family_control = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(field_label(crate::locale::text("字体", "Font"), theme))
            .child(
                Select::new(&font_select)
                    .id("terminal-font-family")
                    .accessibility_label(crate::locale::text("终端字体", "Terminal font"))
                    .search_placeholder(crate::locale::text("搜索字体", "Search fonts"))
                    .disabled(locked)
                    .bg(theme.field_surface())
                    .border_color(theme.strong_border())
                    .rounded(theme.control_radius)
                    .w_full(),
            )
            .children(self.terminal_field_error(
                "font_family",
                SettingsIssueKind::InvalidFontFamily,
                theme,
            ));
        let font_controls = vec![
            number_setting_field(
                crate::locale::text("字号", "Font size"),
                &self.settings_inputs.font_size,
                Some(crate::locale::text("px", "px")),
                None,
                true,
                locked,
                theme,
            )
            .id("terminal-font-size")
            .children(self.terminal_field_error(
                "font_size",
                SettingsIssueKind::InvalidFontSize,
                theme,
            ))
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
            .id("terminal-line-height")
            .children(self.terminal_field_error(
                "line_height",
                SettingsIssueKind::InvalidLineHeight,
                theme,
            ))
            .into_any_element(),
            settings_dropdown(
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
            .into_any_element(),
        ];
        let appearance = subpanel(
            crate::locale::text("外观与字体", "Appearance and font"),
            theme,
        )
        .child(scheme_picker(
            "terminal-scheme",
            terminal.color_scheme,
            is_dark,
            wide_layout,
            locked,
            theme,
            cx,
        ))
        .child(section_separator(theme))
        .child(font_family_control)
        .child(super::render_settings::settings_grid_rows(
            font_controls,
            if medium_layout {
                &[1.0, 1.0, 1.0]
            } else {
                &[1.0]
            },
            super::render_settings::SettingsGridGap::Standard,
            false,
        ))
        .child(font_preview(
            &terminal,
            is_dark,
            resolved_font_family,
            theme,
        ));

        let interaction = subpanel(
            crate::locale::text("操作与连接", "Interaction and connections"),
            theme,
        )
        .child(policy_switch(
            "terminal-selection-copy",
            crate::locale::text("选中即复制", "Copy on selection"),
            Some(crate::locale::text(
                "选中的文本自动复制到剪贴板。",
                "Automatically copy selected text to the clipboard.",
            )),
            terminal.selection_copy,
            locked,
            theme,
            cx.listener(|panel, enabled: &bool, _, cx| {
                panel.edit_settings(|settings| settings.terminal.selection_copy = *enabled, cx);
            }),
        ))
        .child(section_separator(theme))
        .child(settings_dropdown(
            SettingsDropdownSpec {
                id: "terminal-right-click",
                label: crate::locale::text("右键操作", "Right-click action"),
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
        ))
        .child(section_separator(theme))
        .child(policy_switch(
            "terminal-show-tab-numbers",
            crate::locale::text("显示标签序号", "Show tab numbers"),
            None,
            terminal.show_tab_numbers,
            locked,
            theme,
            cx.listener(|panel, enabled: &bool, _, cx| {
                panel.edit_settings(|settings| settings.terminal.show_tab_numbers = *enabled, cx);
            }),
        ))
        .child(section_separator(theme))
        .child(policy_switch(
            "terminal-confirm-close",
            crate::locale::text("关闭标签前确认", "Confirm before closing tab"),
            None,
            terminal.confirm_close_tab,
            locked,
            theme,
            cx.listener(|panel, enabled: &bool, _, cx| {
                panel.edit_settings(
                    |settings| settings.terminal.confirm_close_tab = *enabled,
                    cx,
                );
            }),
        ))
        .child(section_separator(theme))
        .child(policy_switch(
            "terminal-auto-reconnect",
            crate::locale::text("SSH 断线自动重连", "Reconnect SSH automatically"),
            None,
            terminal.auto_reconnect,
            locked,
            theme,
            cx.listener(|panel, enabled: &bool, _, cx| {
                panel.edit_settings(|settings| settings.terminal.auto_reconnect = *enabled, cx);
            }),
        ))
        .when(cfg!(target_os = "macos"), |group| {
            group.child(section_separator(theme)).child(policy_switch(
                "terminal-mac-option-meta",
                crate::locale::text("将 Option 键用作 Meta", "Use Option as Meta"),
                None,
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
        });

        let history = subpanel(crate::locale::text("滚屏历史", "Scrollback history"), theme).child(
            number_setting_field(
                crate::locale::text("保留行数", "Lines to keep"),
                &self.settings_inputs.scrollback,
                Some(crate::locale::text("行", "lines")),
                Some(crate::locale::text(
                    "保留 1,000–50,000 行；更多历史会占用更多内存。",
                    "Keep 1,000–50,000 lines. More history uses more memory.",
                )),
                false,
                locked,
                theme,
            )
            .children(self.terminal_field_error(
                "scrollback",
                SettingsIssueKind::InvalidScrollback,
                theme,
            )),
        );

        let content = div()
            .id("preferences-terminal")
            .flex_1()
            .w_full()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .pr_2()
                    .pb_2()
                    .child(appearance)
                    .child(cursor_section(&terminal, is_dark, locked, theme, cx))
                    .child(interaction)
                    .child(history)
                    .children(
                        self.settings_issues
                            .iter()
                            .filter(|issue| {
                                !matches!(
                                    issue.kind,
                                    SettingsIssueKind::InvalidFontFamily
                                        | SettingsIssueKind::InvalidFontSize
                                        | SettingsIssueKind::InvalidLineHeight
                                        | SettingsIssueKind::InvalidScrollback
                                )
                            })
                            .map(|issue| super::render_settings::settings_issue_row(issue, theme)),
                    )
                    .children(
                        self.settings_input_errors
                            .iter()
                            .filter(|(key, _)| {
                                !matches!(
                                    **key,
                                    "font_family" | "font_size" | "line_height" | "scrollback"
                                )
                            })
                            .map(|(_, message)| super::render_settings::issue_row(message, theme)),
                    )
                    .children(super::render::settings_error_banner(
                        self.settings_error.as_deref(),
                        theme,
                    )),
            );

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(content)
            .child(self.render_terminal_save_status(theme, cx))
            .into_any_element()
    }

    /// 数字格式与领域范围错误就近出现，保留输入中间态；跨页错误仍由正文和状态栏提示。
    /// @author kongweiguang
    fn terminal_field_error(
        &self,
        key: &str,
        kind: SettingsIssueKind,
        theme: ThemeTokens,
    ) -> Option<impl IntoElement> {
        let message = self
            .settings_input_errors
            .get(key)
            .map(String::as_str)
            .or_else(|| {
                self.settings_issues
                    .iter()
                    .find(|issue| issue.kind == kind)
                    .map(|issue| issue.message.as_str())
            })?;
        Some(
            div()
                .id(format!("terminal-{key}-error"))
                .debug_selector({
                    let selector = format!("terminal-{key}-error");
                    move || selector.clone()
                })
                .text_size(theme.text_xs)
                .text_color(theme.error)
                .child(message.to_owned()),
        )
    }

    /// 固定状态区只投影真实草稿状态，输入错误与待保存不显示成功；确定失败才允许调用原保存用例重试。
    /// @author kongweiguang
    fn render_terminal_save_status(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let draft = self.settings.as_ref();
        let state = draft.map(|draft| draft.status());
        let dirty = draft.is_some_and(|draft| draft.is_dirty());
        let invalid = !self.settings_input_errors.is_empty()
            || self.settings_issues.iter().any(|issue| issue.blocks_save());
        let failed = matches!(
            state,
            Some(DraftStatus::Failed | DraftStatus::ExternalConflict | DraftStatus::OutcomeUnknown)
        );
        let saved = state == Some(DraftStatus::Ready)
            && !dirty
            && !invalid
            && self.settings_error.is_none()
            && self.status.is_none()
            && self.settings_success_status == Some(SettingsSuccessStatus::Saved);
        let message = if invalid {
            crate::locale::text(
                "尚未保存，请修正标记的内容。",
                "Not saved. Review the marked fields.",
            )
        } else if failed || self.settings_error.is_some() {
            self.settings_error
                .as_deref()
                .or(self.status.as_deref())
                .map(|message| SharedString::from(message.to_owned()))
                .unwrap_or_else(|| {
                    crate::locale::text(
                        "尚未保存，更改已保留。",
                        "Not saved. Your changes are preserved.",
                    )
                })
        } else if state == Some(DraftStatus::Saving) {
            crate::locale::text("正在保存…", "Saving…")
        } else if dirty {
            crate::locale::text("等待自动保存…", "Waiting to save…")
        } else if let Some(status) = self.status.as_deref() {
            SharedString::from(status.to_owned())
        } else if saved {
            crate::locale::text("设置已保存", "Settings saved")
        } else {
            crate::locale::text("更改将自动保存", "Changes save automatically")
        };
        div()
            .id("terminal-save-status")
            .debug_selector(|| "terminal-save-status".into())
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .pt_2()
            .mt_2()
            .flex()
            .items_center()
            .gap_2()
            .text_size(theme.text_xs)
            .text_color(if invalid || failed || self.settings_error.is_some() {
                theme.error
            } else {
                theme.muted
            })
            .when(saved, |row| {
                row.child(gpui_kit::component::Icon::new(IconName::Check).size_3())
            })
            .child(div().flex_1().min_w_0().child(message))
            .when(
                state == Some(DraftStatus::Failed)
                    && dirty
                    && !invalid
                    && !self.settings_controls_locked(),
                |row| {
                    row.child(
                        Button::new("terminal-retry-save")
                            .small()
                            .outline()
                            .label(crate::locale::text("重试保存", "Retry save"))
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.save_settings(cx);
                            })),
                    )
                },
            )
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

/// 同层级设置使用统一中性分组与标题，不再用装饰图标或等高列暗示不存在的关联。
///
/// @author kongweiguang
fn subpanel(title: SharedString, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .min_w_0()
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
                .text_size(theme.text_sm)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.foreground)
                .child(title),
        )
}

/// 字段标题与原生控件共用一条对齐线，避免每个字段派生不同层级。
/// @author kongweiguang
fn field_label(label: SharedString, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .text_size(theme.text_xs)
        .font_weight(theme.settings_medium_font_weight())
        .text_color(theme.field_label)
        .child(label)
}

/// 分隔线归分组所有，不给每一设置行再套一层卡片。
/// @author kongweiguang
fn section_separator(theme: ThemeTokens) -> gpui_kit::Div {
    div().w_full().border_b_1().border_color(theme.border)
}

/// 单一主题按当前应用明暗显示实际色样，并保持键盘选择和稳定身份。
///
/// @author kongweiguang
fn scheme_picker(
    id_prefix: &'static str,
    current: TerminalColorScheme,
    is_dark: bool,
    wide: bool,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    div().w_full().flex().flex_col().gap_2().children(
        SCHEME_OPTIONS
            .chunks(if wide { 4 } else { 2 })
            .map(|row_options| {
                div().w_full().min_w(px(0.)).flex().gap_2().children(
                    row_options.iter().copied().map(|option| {
                        let selected = option.value == current;
                        let value = option.value;
                        let colors = terminal_preview_colors(value, is_dark);
                        let swatches = [
                            colors.background,
                            colors.foreground,
                            colors.green,
                            colors.cyan,
                        ];
                        Button::new(format!("{id_prefix}-{}", option.id))
                            .flex_1()
                            .min_w(px(0.))
                            .h_16()
                            .px_3()
                            .rounded(theme.control_radius)
                            .border_1()
                            .border_color(if selected {
                                theme.accent.opacity(0.55)
                            } else {
                                theme.border
                            })
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(if selected {
                                        theme.surface_selected()
                                    } else {
                                        theme.secondary
                                    })
                                    .foreground(theme.foreground)
                                    .hover(theme.surface_hover())
                                    .active(theme.surface_selected()),
                            )
                            .selected(selected)
                            .toggled(selected)
                            .focus_ring(true)
                            .disabled(disabled)
                            .accessibility_label(crate::locale::text(
                                format!("{} 终端主题", option.label),
                                format!("{} terminal theme", option.label),
                            ))
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
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .truncate()
                                                    .text_size(theme.text_sm)
                                                    .child(option.label),
                                            )
                                            .child(
                                                gpui_kit::component::Icon::new(IconName::Check)
                                                    .size_3()
                                                    .opacity(if selected { 1.0 } else { 0.0 }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .overflow_hidden()
                                            .rounded_full()
                                            .border_1()
                                            .border_color(theme.border)
                                            .children(swatches.into_iter().map(|color| {
                                                div().flex_1().h_2().bg(gpui_kit::rgb(color))
                                            })),
                                    ),
                            )
                            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                panel.edit_settings(
                                    |settings| set_terminal_scheme(settings, value),
                                    cx,
                                );
                            }))
                    }),
                )
            }),
    )
}
/// 单一主题同步旧版浅深字段，避免旧配置读取者与当前选择产生不同结果。
///
/// @author kongweiguang
fn set_terminal_scheme(settings: &mut SettingsSnapshot, value: TerminalColorScheme) {
    settings.terminal.color_scheme = value;
    settings.terminal.dark_color_scheme = value;
    settings.terminal.light_color_scheme = value;
    settings.terminal.legacy_color_scheme_missing = false;
}

/// 字号与行高复用同一 InputState 的边界、步进和中间态，其余字段沿用普通输入框。
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
) -> gpui_kit::Div {
    let accessibility_label = label.clone();
    let field = if compact_controls {
        NumberInput::new(input)
            .small()
            .h(px(36.))
            .w_full()
            .bg(theme.field_surface())
            .border_color(theme.strong_border())
            .rounded(theme.control_radius)
            .disabled(disabled)
            .when_some(suffix, |field, suffix| field.suffix(suffix))
            .into_any_element()
    } else {
        gpui_kit::Styled::h(
            super::render_settings::settings_input(input, theme),
            px(36.),
        )
        .w_full()
        .disabled(disabled)
        .when_some(suffix, |field, suffix| field.suffix(suffix))
        .into_any_element()
    };
    div()
        .flex_1()
        .min_w(px(if compact_controls { 120. } else { 112. }))
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
            div()
                .id(("number-setting-group", input.entity_id()))
                .w_full()
                .role(gpui_kit::Role::Group)
                .aria_label(accessibility_label)
                .child(field),
        )
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

/// 将用户选择的字重写入 snapshot，字体预览和运行时终端读取同一字段。
///
/// @author kongweiguang
fn set_font_weight(settings: &mut SettingsSnapshot, value: TerminalFontWeight) {
    settings.terminal.font_weight = value;
}

/// 用唯一终端样本同时反馈配色、字体和光标；字重直接复用运行时映射，避免展示比实际终端更粗的文本。
///
/// @author kongweiguang
fn font_preview(
    terminal: &kerminal_application::preferences::TerminalSettings,
    is_dark: bool,
    resolved_font_family: SharedString,
    theme: ThemeTokens,
) -> impl IntoElement {
    let colors = terminal_preview_colors(terminal.color_scheme, is_dark);
    // 预览模拟终端栅格，因此保留与运行时相同的物理字号和行高，不跟随界面 rem 二次缩放。
    let font_size = px(f32::from(terminal.font_size));
    let line_height = px(f32::from(terminal.font_size) * terminal.line_height as f32);
    let weight = font_weight(terminal.font_weight);
    div()
        .id("terminal-font-preview")
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .rounded(theme.control_radius)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text("终端预览", "Terminal preview"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_right()
                        .child(resolved_font_family.clone()),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .rounded_b(theme.control_radius)
                .bg(gpui_kit::rgb(colors.background))
                .px_3()
                .py_3()
                .children([
                    preview_line(
                        "$ ssh user@server.example.com".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        weight,
                        gpui_kit::rgb(colors.green).into(),
                    ),
                    preview_line(
                        "中文路径 /var/log/app · 0123456789".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        weight,
                        gpui_kit::rgb(colors.foreground).into(),
                    ),
                    preview_line(
                        "Aa Bb Cc  Il1 O0  {} [] <>".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        weight,
                        gpui_kit::rgb(colors.foreground).into(),
                    ),
                    preview_line(
                        "$ ".to_owned(),
                        &resolved_font_family,
                        font_size,
                        line_height,
                        weight,
                        gpui_kit::rgb(colors.cyan).into(),
                    )
                    .flex()
                    .items_center()
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

/// 光标形态与闪烁就近组合；静态选项只表达形状，唯一终端预览展示闪烁，避免多个动画争夺注意力。
///
/// @author kongweiguang
fn cursor_section(
    terminal: &kerminal_application::preferences::TerminalSettings,
    is_dark: bool,
    locked: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    let colors = terminal_preview_colors(terminal.color_scheme, is_dark);
    let choices = [
        (TerminalCursorStyle::Block, "cursor-block", "块状", "Block"),
        (TerminalCursorStyle::Bar, "cursor-bar", "竖线", "Bar"),
        (
            TerminalCursorStyle::Underline,
            "cursor-underline",
            "下划线",
            "Underline",
        ),
    ];
    let cards = choices
        .into_iter()
        .map(|(style, id, label_zh, label_en)| {
            let animation_id = match style {
                TerminalCursorStyle::Block => "terminal-cursor-preview-block",
                TerminalCursorStyle::Bar => "terminal-cursor-preview-bar",
                TerminalCursorStyle::Underline => "terminal-cursor-preview-underline",
            };
            let selected = terminal.cursor_style == style;
            Button::new(format!("terminal-{id}"))
                .compact()
                .w(rems(8.25))
                .flex_shrink_0()
                .min_w(px(0.))
                .h_8()
                .px_2()
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
                .selected(selected)
                .toggled(selected)
                .disabled(locked)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .size(px(22.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme.radius_sm)
                                .bg(gpui_kit::rgb(colors.background))
                                .child(cursor_marker(style, colors.cursor, false, animation_id)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_size(theme.text_sm)
                                .child(crate::locale::text(label_zh, label_en)),
                        )
                        .when(selected, |row| {
                            row.child(gpui_kit::component::Icon::new(IconName::Check).size(px(14.)))
                        }),
                )
                .accessibility_label(crate::locale::text(
                    format!("{label_zh}光标"),
                    format!("{label_en} cursor"),
                ))
                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                    panel.edit_settings(move |settings| settings.terminal.cursor_style = style, cx);
                }))
                .into_any_element()
        })
        .collect::<Vec<_>>();
    let cards = div()
        .id("terminal-cursor-choices")
        .flex()
        .flex_wrap()
        .gap_2()
        .children(cards);
    subpanel(crate::locale::text("光标", "Cursor"), theme)
        .child(cards)
        .child(section_separator(theme))
        .child(policy_switch(
            "terminal-cursor-blink",
            crate::locale::text("光标闪烁", "Blink cursor"),
            None,
            terminal.cursor_blink,
            locked,
            theme,
            cx.listener(|panel, enabled: &bool, _, cx| {
                panel.edit_settings(|settings| settings.terminal.cursor_blink = *enabled, cx);
            }),
        ))
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

/// 平整的设置行保留整行热区；子 Switch 截断冒泡并提供键盘与禁用语义。
///
/// @author kongweiguang
fn policy_switch(
    id: &'static str,
    label: SharedString,
    description: Option<SharedString>,
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
        .debug_selector(move || format!("{id}-row"))
        .w_full()
        .min_w(px(0.))
        .min_h(rems(2.75))
        .py_1()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .whitespace_normal()
                        .text_left()
                        .text_color(theme.foreground)
                        .child(label),
                )
                .when_some(description, |column, description| {
                    column.child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(description),
                    )
                }),
        )
        .child(
            Switch::new(id)
                .small()
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
            row.on_click(move |_, window, cx| {
                listener(&!checked, window, cx);
            })
        })
}
