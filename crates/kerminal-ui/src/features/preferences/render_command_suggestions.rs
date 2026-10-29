//! main 命令建议策略页的单卡片、双栏设置视图。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, Entity, IntoElement, Window,
    assets::IconName,
    component::{Disableable, Icon, select::Select, switch::Switch},
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::{
    TerminalCommandSuggestionPresentation, TerminalCommandSuggestionRemoteRefresh,
};

use crate::theme::ThemeTokens;

use super::{panel::PreferencesPanel, panel::terminal::SuggestionSelectOption};

#[derive(Clone, Copy)]
enum SuggestionProvider {
    History,
    RemotePath,
    RemoteCommand,
    Git,
    Spec,
}

/// Provider metadata and enablement travel together so the shared row helper has one semantic input.
struct ProviderToggleConfig {
    provider: SuggestionProvider,
    id: &'static str,
    icon: IconName,
    label: gpui_kit::SharedString,
    checked: bool,
    disabled: bool,
}

impl PreferencesPanel {
    /// 策略区按 main 的双列网格排列，右侧 Provider 仅分配 0.72 份宽度，避免挤压接受策略。
    /// 使用 main 的视口媒体断点；窄窗移除零 basis，防止纵向堆叠时卡片高度塌缩。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn render_command_suggestions(
        &mut self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return div()
                .id("settings-suggestions-panel")
                .w_full()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&self.content_scroll)
                .child(
                    div()
                        .id("command-suggestions-loading")
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "正在读取命令提示设置…",
                            "Loading command suggestion settings…",
                        )),
                )
                .child(self.render_save_notice(theme));
        };

        let wide = window.viewport_size().width >= px(1280.);
        let medium = window.viewport_size().width >= px(768.);
        // main 的 auto-fit 以 150px 列和 8px 间距填充卡片；壳宽、侧栏和留白沿用同一固定几何。
        let provider_width =
            (window.viewport_size().width.as_f32() - 48.).min(1152.) - 40. - 190. - 16. - 32.;
        let provider_columns = if wide {
            1
        } else {
            ((provider_width + 8.) / 158.).floor().clamp(1., 5.) as u16
        };
        let suggestion = settings.terminal.inline_suggestion;
        let locked = self.settings_controls_locked();
        let header = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .pb_4()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .min_w(px(0.))
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(px(16.))
                            .text_color(theme.muted),
                    )
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .font_weight(theme.settings_medium_font_weight())
                            .text_color(theme.foreground)
                            .child(crate::locale::text("命令灰色提示", "Command suggestions")),
                    ),
            )
            .child(
                div()
                    .min_w(px(220.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(if suggestion.enabled {
                        theme.accent.opacity(0.3)
                    } else {
                        theme.strong_border()
                    })
                    .bg(if suggestion.enabled {
                        theme.surface_selected()
                    } else {
                        theme.field_surface()
                    })
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(if suggestion.enabled {
                                theme.accent
                            } else {
                                theme.muted
                            })
                            .child(crate::locale::text(
                                if suggestion.enabled {
                                    "已启用"
                                } else {
                                    "已暂停"
                                },
                                if suggestion.enabled {
                                    "Enabled"
                                } else {
                                    "Paused"
                                },
                            )),
                    )
                    .child(
                        Switch::new("suggestions-enabled")
                            .track_size(px(40.), px(24.))
                            .checked(suggestion.enabled)
                            .disabled(locked)
                            .accessibility_label(crate::locale::text(
                                "启用灰色提示",
                                "Enable command suggestions",
                            ))
                            .on_change(cx.listener(|panel, enabled: &bool, window, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        let suggestion = &mut settings.terminal.inline_suggestion;
                                        suggestion.enabled = *enabled;
                                        suggestion.presentation = if *enabled {
                                            TerminalCommandSuggestionPresentation::InlineAndMenu
                                        } else {
                                            TerminalCommandSuggestionPresentation::Off
                                        };
                                    },
                                    cx,
                                );
                                if let Some(settings) =
                                    panel.settings.as_ref().map(|draft| draft.value().clone())
                                {
                                    panel
                                        .suggestion_selects
                                        .sync_from_settings(&settings, window, cx);
                                }
                            })),
                    ),
            );

        let policy_controls = div()
            .w_full()
            .grid()
            .grid_cols(if medium { 2 } else { 1 })
            .gap_2()
            .child(select_field(
                crate::locale::text("展示方式", "Display mode"),
                crate::locale::text("命令建议展示方式", "Command suggestion display mode"),
                "suggestion-presentation",
                &self.suggestion_selects.presentation,
                locked,
                theme,
            ))
            .child(select_field(
                crate::locale::text("接受按键", "Accept key"),
                crate::locale::text("灰色提示接受按键", "Command suggestion accept key"),
                "suggestion-accept-key",
                &self.suggestion_selects.accept_key,
                locked,
                theme,
            ))
            .child(select_field(
                crate::locale::text("远端刷新", "Remote refresh"),
                crate::locale::text("命令建议远端刷新", "Command suggestion remote refresh"),
                "suggestion-remote-refresh",
                &self.suggestion_selects.remote_refresh,
                locked,
                theme,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(190.))
                    .min_h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.strong_border())
                    .bg(theme.field_surface())
                    .px_3()
                    .py_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.foreground)
                    .child(crate::locale::text(
                        "允许 Alt+Right 分段接受",
                        "Allow partial acceptance with Alt+Right",
                    ))
                    .child(
                        Switch::new("suggestion-partial-accept")
                            .track_size(px(40.), px(24.))
                            .checked(suggestion.partial_accept)
                            .disabled(locked)
                            .accessibility_label(crate::locale::text(
                                "允许分段接受命令建议",
                                "Allow partial command suggestion acceptance",
                            ))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.terminal.inline_suggestion.partial_accept =
                                            *enabled;
                                    },
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(190.))
                    .min_h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.strong_border())
                    .bg(theme.field_surface())
                    .px_3()
                    .py_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.foreground)
                    .child(crate::locale::text(
                        "允许 Tab 打开候选列表",
                        "Allow Tab to open the suggestion list",
                    ))
                    .child(
                        Switch::new("suggestion-tab-opens-menu")
                            .track_size(px(40.), px(24.))
                            .checked(suggestion.tab_opens_menu)
                            .disabled(locked)
                            .accessibility_label(crate::locale::text(
                                "允许 Tab 打开命令建议列表",
                                "Allow Tab to open command suggestions",
                            ))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.terminal.inline_suggestion.tab_opens_menu =
                                            *enabled;
                                    },
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .col_span_full()
                    .min_h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.strong_border())
                    .bg(theme.field_surface())
                    .px_3()
                    .py_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Network)
                                    .size(px(16.))
                                    .text_color(theme.muted),
                            )
                            .child(crate::locale::text(
                                "允许远端只读探测",
                                "Allow read-only remote probes",
                            )),
                    )
                    .child(
                        Switch::new("suggestion-remote-probe")
                            .track_size(px(40.), px(24.))
                            .checked(suggestion.remote_probe_enabled)
                            .disabled(locked)
                            .accessibility_label(crate::locale::text(
                                "允许远端只读探测",
                                "Allow read-only remote probes",
                            ))
                            .on_change(cx.listener(|panel, enabled: &bool, window, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        let suggestion = &mut settings.terminal.inline_suggestion;
                                        suggestion.remote_probe_enabled = *enabled;
                                        suggestion.remote_refresh = if *enabled {
                                            TerminalCommandSuggestionRemoteRefresh::Safe
                                        } else {
                                            TerminalCommandSuggestionRemoteRefresh::Off
                                        };
                                    },
                                    cx,
                                );
                                if let Some(settings) =
                                    panel.settings.as_ref().map(|draft| draft.value().clone())
                                {
                                    panel
                                        .suggestion_selects
                                        .sync_from_settings(&settings, window, cx);
                                }
                            })),
                    ),
            );

        let status_top = div()
            .w_full()
            .flex()
            .gap_2()
            .child(suggestion_status_tile(
                IconName::ShieldCheck,
                crate::locale::text("主机安装", "Host installation"),
                crate::locale::text("不需要插件", "No plugin required"),
                theme.success,
                theme,
            ))
            .child(suggestion_status_tile(
                IconName::Network,
                crate::locale::text("远端探测", "Remote probes"),
                crate::locale::text(
                    if suggestion.remote_probe_enabled {
                        "后台只读"
                    } else {
                        "已关闭"
                    },
                    if suggestion.remote_probe_enabled {
                        "Read-only in background"
                    } else {
                        "Off"
                    },
                ),
                if suggestion.remote_probe_enabled {
                    theme.accent
                } else {
                    theme.muted
                },
                theme,
            ));
        let status_bottom = div()
            .w_full()
            .flex()
            .gap_2()
            .child(suggestion_status_tile(
                IconName::Activity,
                crate::locale::text("反馈调权", "Feedback weighting"),
                crate::locale::text(
                    if suggestion.enabled {
                        "接受/忽略"
                    } else {
                        "暂停"
                    },
                    if suggestion.enabled {
                        "Accepted / ignored"
                    } else {
                        "Paused"
                    },
                ),
                if suggestion.enabled {
                    theme.accent
                } else {
                    theme.muted
                },
                theme,
            ))
            .child(div().flex_1());

        let policy = div()
            .when(wide, |policy| {
                policy.flex_basis(px(0.)).flex_grow(1.).min_w(px(340.))
            })
            .when(!wide, |policy| policy.w_full().min_w(px(0.)))
            .child(section_heading(
                IconName::ShieldCheck,
                crate::locale::text("策略与接受", "Policy and acceptance"),
                theme,
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .mt_3()
                    .child(policy_controls),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .mt_3()
                    .child(status_top)
                    .child(status_bottom),
            );

        let provider_specs = [
            ProviderToggleConfig {
                provider: SuggestionProvider::History,
                id: "suggestion-provider-history",
                icon: IconName::Clipboard,
                label: crate::locale::text("历史", "History"),
                checked: suggestion.providers.history,
                disabled: locked,
            },
            ProviderToggleConfig {
                provider: SuggestionProvider::RemotePath,
                id: "suggestion-provider-remote-path",
                icon: IconName::Route,
                label: crate::locale::text("远端路径", "Remote paths"),
                checked: suggestion.providers.remote_path,
                disabled: locked,
            },
            ProviderToggleConfig {
                provider: SuggestionProvider::RemoteCommand,
                id: "suggestion-provider-remote-command",
                icon: IconName::Terminal,
                label: crate::locale::text("远端命令", "Remote commands"),
                checked: suggestion.providers.remote_command,
                disabled: locked,
            },
            ProviderToggleConfig {
                provider: SuggestionProvider::Git,
                id: "suggestion-provider-git",
                icon: IconName::GitBranch,
                label: crate::locale::text("Git", "Git"),
                checked: suggestion.providers.git,
                disabled: locked,
            },
            ProviderToggleConfig {
                provider: SuggestionProvider::Spec,
                id: "suggestion-provider-spec",
                icon: IconName::Wrench,
                label: crate::locale::text("CLI Spec", "CLI Spec"),
                checked: suggestion.providers.spec,
                disabled: locked,
            },
        ];
        let mut provider_rows = div().w_full().grid().grid_cols(provider_columns).gap_2();
        for provider in provider_specs {
            provider_rows = provider_rows.child(provider_toggle(provider, theme, cx));
        }
        let providers = div()
            .id("settings-command-suggestions-providers-panel")
            .when(wide, |providers| {
                providers.min_w(px(260.)).flex_basis(px(0.)).flex_grow(0.72)
            })
            .when(!wide, |providers| providers.w_full().min_w(px(0.)))
            .child(section_heading(
                IconName::Puzzle,
                crate::locale::text("Provider 开关", "Provider toggles"),
                theme,
            ))
            .child(provider_rows.mt_3());

        div()
            .id("settings-suggestions-panel")
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_4()
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .child(
                div()
                    .id("settings-command-suggestions-policy-panel")
                    .w_full()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(header)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .when(!wide, |row| row.flex_col())
                            .items_start()
                            .gap_4()
                            .child(policy)
                            .child(providers),
                    ),
            )
            .child(self.render_save_notice(theme))
    }
}

/// Select 字段使用 main 的紧凑标签在上、当前值在下布局。
///
/// @author kongweiguang
fn select_field(
    label: gpui_kit::SharedString,
    accessibility_label: gpui_kit::SharedString,
    id: &'static str,
    state: &Entity<gpui_kit::component::select::SelectState<Vec<SuggestionSelectOption>>>,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(190.))
        .rounded_md()
        .border_1()
        .border_color(theme.strong_border())
        .bg(theme.field_surface())
        .px_3()
        .py_2()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            Select::new(state)
                .bg(theme.field_surface())
                .border_color(theme.strong_border())
                .rounded(theme.control_radius)
                .id(id)
                .accessibility_label(accessibility_label)
                .disabled(disabled)
                .w_full(),
        )
}

/// 小标题和图标与 main 的策略/provider 分区保持同一层级。
///
/// @author kongweiguang
fn section_heading(
    icon: IconName,
    label: gpui_kit::SharedString,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(theme.text_xs)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(theme.field_label)
        .child(Icon::new(icon).size(px(14.)).text_color(theme.muted))
        .child(label)
}

/// 状态摘要同时展示状态来源和当前值，并用语义色区别活跃、暂停与无需配置。
///
/// @author kongweiguang
fn suggestion_status_tile(
    icon: IconName,
    label: gpui_kit::SharedString,
    value: gpui_kit::SharedString,
    tone: gpui_kit::Hsla,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(150.))
        .min_h(px(36.))
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(theme.strong_border())
        .bg(theme.field_surface())
        .px_2()
        .py_2()
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .flex()
                .items_center()
                .gap_2()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(Icon::new(icon).size(px(14.)).text_color(tone))
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .rounded_full()
                .bg(tone.opacity(0.12))
                .px_2()
                .py_1()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(tone)
                .child(value),
        )
}

/// Provider 开关按 main 的图标、来源名和受控 Switch 行呈现。
///
/// @author kongweiguang
fn provider_toggle(
    config: ProviderToggleConfig,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    let ProviderToggleConfig {
        provider,
        id,
        icon,
        label,
        checked,
        disabled,
    } = config;
    let accessibility_label = label.clone();
    div()
        .min_h(px(40.))
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(theme.strong_border())
        .bg(theme.field_surface())
        .px_3()
        .py_2()
        .text_size(theme.text_sm)
        .text_color(theme.field_label)
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).size(px(16.)).text_color(theme.muted))
                .child(label),
        )
        .child(
            Switch::new(id)
                .track_size(px(40.), px(24.))
                .checked(checked)
                .disabled(disabled)
                .accessibility_label(accessibility_label)
                .on_change(cx.listener(move |panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| {
                            let providers = &mut settings.terminal.inline_suggestion.providers;
                            match provider {
                                SuggestionProvider::History => providers.history = *enabled,
                                SuggestionProvider::RemotePath => providers.remote_path = *enabled,
                                SuggestionProvider::RemoteCommand => {
                                    providers.remote_command = *enabled
                                }
                                SuggestionProvider::Git => providers.git = *enabled,
                                SuggestionProvider::Spec => providers.spec = *enabled,
                            }
                        },
                        cx,
                    );
                })),
        )
}
