//! 终端输入行为、建议与关键词规则的原生编辑区。
//!
//! @author kongweiguang

use std::fmt::Debug;

use gpui_kit::{
    ClickEvent, Context, IntoElement,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::Input,
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::{
    SettingsSnapshot, TerminalColorScheme, TerminalCommandSuggestionPresentation,
    TerminalCommandSuggestionRemoteRefresh, TerminalFontWeight, TerminalInlineSuggestionAcceptKey,
    TerminalInlineSuggestionProviderSettings, TerminalKeywordHighlightMatchMode,
    TerminalKeywordHighlightStyle, TerminalRightClickBehavior,
};

use crate::theme::ThemeTokens;

use super::panel::PreferencesPanel;

impl PreferencesPanel {
    /// 把旧版终端行为配置映射到 typed 快照控件，保存统一由父页面 CAS 提交。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn render_terminal_behavior(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return div()
                .id("terminal-behavior-loading")
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "正在读取终端设置…",
                    "Loading terminal preferences…",
                ));
        };
        let terminal = settings.terminal;
        let suggestion = terminal.inline_suggestion;
        let keywords = terminal.keyword_highlights;
        let locked = self.settings_controls_locked();

        let keyword_rows = keywords
            .rules
            .iter()
            .map(|rule| {
                let rule_id = rule.id.clone();
                let enabled_rule_id = rule.id.clone();
                let delete_rule_id = rule.id.clone();
                let case_sensitive_rule_id = rule.id.clone();
                let selected_mode = rule.match_mode;
                let selected_style = rule.style;
                let enabled = rule.enabled;
                let case_sensitive = rule.case_sensitive;
                let custom_colors = rule.custom_colors.is_some();
                let inputs = self
                    .terminal_behavior_inputs
                    .keyword_rules
                    .iter()
                    .find(|inputs| inputs.id == rule.id);
                let mode_buttons = keyword_mode_buttons(&rule_id, selected_mode, locked, cx);
                let style_buttons =
                    keyword_style_buttons(&rule_id, selected_style, locked, custom_colors, cx);
                let mut card = div()
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(theme.panel)
                    .rounded_md()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().child(crate::locale::text(
                                format!("规则：{}", rule.id),
                                format!("Rule: {}", rule.id),
                            )))
                            .child(
                                Switch::new(format!("keyword-enabled-{}", rule.id))
                                    .checked(enabled)
                                    .disabled(locked)
                                    .label(crate::locale::text("启用", "Enabled"))
                                    .on_change(cx.listener(move |panel, enabled: &bool, _, cx| {
                                        panel.update_keyword_rule(
                                            &enabled_rule_id,
                                            |rule| rule.enabled = *enabled,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(format!("keyword-delete-{}", rule.id))
                                    .compact()
                                    .label(crate::locale::text("删除规则", "Delete rule"))
                                    .disabled(locked)
                                    .on_click(cx.listener(
                                        move |panel, _: &ClickEvent, window, cx| {
                                            panel.delete_keyword_rule(&delete_rule_id, window, cx);
                                        },
                                    )),
                            ),
                    );
                if let Some(inputs) = inputs {
                    card = card
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text("匹配内容", "Pattern")),
                        )
                        .child(Input::new(&inputs.pattern).disabled(locked).w(px(520.)))
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text("备注", "Note")),
                        )
                        .child(Input::new(&inputs.note).disabled(locked).w(px(520.)))
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text("浅色主题颜色", "Light theme colors")),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .child(crate::locale::text("前景", "Foreground")),
                                )
                                .child(
                                    Input::new(&inputs.light_foreground)
                                        .disabled(locked)
                                        .w(px(130.)),
                                )
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .child(crate::locale::text("背景", "Background")),
                                )
                                .child(
                                    Input::new(&inputs.light_background)
                                        .disabled(locked)
                                        .w(px(130.)),
                                ),
                        )
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text("深色主题颜色", "Dark theme colors")),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .child(crate::locale::text("前景", "Foreground")),
                                )
                                .child(
                                    Input::new(&inputs.dark_foreground)
                                        .disabled(locked)
                                        .w(px(130.)),
                                )
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .child(crate::locale::text("背景", "Background")),
                                )
                                .child(
                                    Input::new(&inputs.dark_background)
                                        .disabled(locked)
                                        .w(px(130.)),
                                ),
                        );
                } else {
                    card = card.child(div().text_color(theme.muted).child(crate::locale::text(
                        "规则输入正在准备…",
                        "Preparing rule inputs…",
                    )));
                }
                card.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("匹配方式", "Match mode")),
                )
                .children(mode_buttons)
                .child(
                    Switch::new(format!("keyword-case-{}", rule.id))
                        .checked(case_sensitive)
                        .disabled(locked)
                        .label(crate::locale::text("区分大小写", "Case sensitive"))
                        .on_change(cx.listener(move |panel, enabled: &bool, _, cx| {
                            panel.update_keyword_rule(
                                &case_sensitive_rule_id,
                                |rule| rule.case_sensitive = *enabled,
                                cx,
                            );
                        })),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("高亮预设", "Highlight preset")),
                )
                .children(style_buttons)
            })
            .collect::<Vec<_>>();

        let mut body = div()
            .id("terminal-behavior-settings")
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                section_card(crate::locale::text("连接与交互", "Connection and interaction"), theme)
                    .child(
                        Switch::new("terminal-auto-reconnect")
                            .checked(terminal.auto_reconnect)
                            .disabled(locked)
                            .label(crate::locale::text("断线后自动重连", "Reconnect after disconnect"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.terminal.auto_reconnect = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("terminal-mac-option-meta")
                            .checked(terminal.mac_option_is_meta)
                            .disabled(locked)
                            .label(crate::locale::text("macOS Option 键作为 Meta", "Use macOS Option as Meta"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.terminal.mac_option_is_meta = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("terminal-command-block-rail")
                            .checked(terminal.show_command_block_rail)
                            .disabled(locked)
                            .label(crate::locale::text("显示命令块标记", "Show command block markers"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.terminal.show_command_block_rail = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("terminal-tab-numbers")
                            .checked(terminal.show_tab_numbers)
                            .disabled(locked)
                            .label(crate::locale::text("标签显示序号", "Show tab numbers"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.terminal.show_tab_numbers = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("terminal-confirm-close")
                            .checked(terminal.confirm_close_tab)
                            .disabled(locked)
                            .label(crate::locale::text("关闭终端标签前确认", "Confirm before closing terminal tabs"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.terminal.confirm_close_tab = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(enum_row(
                        crate::locale::text("文本字重", "Font weight"),
                        terminal.font_weight,
                        &[
                            (TerminalFontWeight::Normal, crate::locale::text("常规", "Regular")),
                            (TerminalFontWeight::Medium, crate::locale::text("中等", "Medium")),
                            (TerminalFontWeight::Bold, crate::locale::text("粗体", "Bold")),
                        ],
                        locked,
                        cx,
                        set_terminal_font_weight,
                    ))
                    .child(enum_row(
                        crate::locale::text("右键动作", "Right-click action"),
                        terminal.right_click_behavior,
                        &[
                            (TerminalRightClickBehavior::Menu, crate::locale::text("显示菜单", "Show menu")),
                            (TerminalRightClickBehavior::Paste, crate::locale::text("粘贴", "Paste")),
                            (TerminalRightClickBehavior::None, crate::locale::text("无动作", "Do nothing")),
                        ],
                        locked,
                        cx,
                        set_terminal_right_click,
                    )),
            )
            .child(
                section_card(crate::locale::text("浅色与深色终端配色", "Light and dark terminal palettes"), theme)
                    .child(enum_row(
                        crate::locale::text("浅色主题配色", "Light theme palette"),
                        terminal.light_color_scheme,
                        &scheme_options(),
                        locked,
                        cx,
                        set_light_color_scheme,
                    ))
                    .child(enum_row(
                        crate::locale::text("深色主题配色", "Dark theme palette"),
                        terminal.dark_color_scheme,
                        &scheme_options(),
                        locked,
                        cx,
                        set_dark_color_scheme,
                    ))
                    .child(div().text_size(theme.text_xs).text_color(theme.muted).child(
                        crate::locale::text("当前通用配色仍用于旧配置回退；窗口运行时根据主题选择对应配色。", "The general palette remains as a legacy fallback; the active window selects the matching palette for its theme."),
                    )),
            )
            .child(
                section_card(crate::locale::text("命令建议", "Command suggestions"), theme)
                    .child(
                        Switch::new("suggestions-enabled")
                            .checked(suggestion.enabled)
                            .disabled(locked)
                            .label(crate::locale::text("启用命令建议", "Enable command suggestions"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.set_suggestion_enabled(*enabled, cx);
                            })),
                    )
                    .child(enum_row(
                        crate::locale::text("建议显示方式", "Suggestion display"),
                        suggestion.presentation,
                        &[
                            (TerminalCommandSuggestionPresentation::Off, crate::locale::text("关闭", "Off")),
                            (TerminalCommandSuggestionPresentation::Inline, crate::locale::text("行内", "Inline")),
                            (
                                TerminalCommandSuggestionPresentation::InlineAndMenu,
                                crate::locale::text("行内和菜单", "Inline and menu"),
                            ),
                        ],
                        locked,
                        cx,
                        set_suggestion_presentation,
                    ))
                    .child(enum_row(
                        crate::locale::text("接受行内建议", "Accept inline suggestion"),
                        suggestion.accept_key,
                        &[
                            (TerminalInlineSuggestionAcceptKey::Disabled, crate::locale::text("关闭", "Off")),
                            (TerminalInlineSuggestionAcceptKey::RightArrow, crate::locale::text("右方向键", "Right arrow")),
                        ],
                        locked || !suggestion.enabled,
                        cx,
                        set_suggestion_accept_key,
                    ))
                    .child(
                        Switch::new("suggestion-tab-menu")
                            .checked(suggestion.tab_opens_menu)
                            .disabled(locked || !suggestion.enabled)
                            .label(crate::locale::text("Tab 打开候选菜单", "Open suggestion menu with Tab"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.terminal.inline_suggestion.tab_opens_menu = *enabled
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("suggestion-partial-accept")
                            .checked(suggestion.partial_accept)
                            .disabled(locked || !suggestion.enabled)
                            .label(crate::locale::text("允许逐段接受建议", "Allow accepting suggestions in parts"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.terminal.inline_suggestion.partial_accept = *enabled
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(enum_row(
                        crate::locale::text("远端安全刷新", "Safe remote refresh"),
                        suggestion.remote_refresh,
                        &[
                            (TerminalCommandSuggestionRemoteRefresh::Off, crate::locale::text("关闭", "Off")),
                            (TerminalCommandSuggestionRemoteRefresh::Safe, crate::locale::text("安全刷新", "Safe refresh")),
                        ],
                        locked || !suggestion.enabled,
                        cx,
                        set_remote_refresh,
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(190.)).text_size(px(12.)).child(crate::locale::text("候选菜单快捷键", "Suggestion menu shortcut")))
                            .child(div().text_size(px(12.)).child("Ctrl+Space")),
                    )
                    .child(provider_switches(
                        &suggestion.providers,
                        !suggestion.enabled || locked,
                        theme,
                        cx,
                    ))
                    .child(
                        Switch::new("suggestion-remote-probe")
                            .checked(suggestion.remote_probe_enabled)
                            .disabled(locked || !suggestion.enabled)
                            .label(crate::locale::text("允许安全远端探测", "Allow safe remote probes"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        let suggestion = &mut settings.terminal.inline_suggestion;
                                        suggestion.remote_probe_enabled = *enabled;
                                        if !enabled {
                                            suggestion.remote_refresh =
                                                TerminalCommandSuggestionRemoteRefresh::Off;
                                        } else if suggestion.remote_refresh
                                            == TerminalCommandSuggestionRemoteRefresh::Off
                                        {
                                            suggestion.remote_refresh =
                                                TerminalCommandSuggestionRemoteRefresh::Safe;
                                        }
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(number_field(
                        crate::locale::text("审计数据保留（天）", "Audit retention (days)"),
                        &self.terminal_behavior_inputs.audit_retention_days,
                        locked,
                    ))
                    .child(number_field(
                        crate::locale::text("反馈数据保留（天）", "Feedback retention (days)"),
                        &self.terminal_behavior_inputs.feedback_retention_days,
                        locked,
                    ))
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "保留天数变更会在下次启动时执行清理。",
                                "Retention changes are applied during cleanup at the next launch.",
                            )),
                    ),
            )
            .child(
                section_card(crate::locale::text("关键词高亮", "Keyword highlighting"), theme)
                    .child(
                        Switch::new("keyword-highlights-enabled")
                            .checked(keywords.enabled)
                            .disabled(locked)
                            .label(crate::locale::text("启用关键词高亮", "Enable keyword highlighting"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.terminal.keyword_highlights.enabled = *enabled
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(div().text_size(theme.text_xs).text_color(theme.muted).child(
                        crate::locale::text("自定义颜色分别设置浅色和深色主题的前景色、背景色；正则在保存前由用例限制复杂回溯。", "Set separate foreground and background colors for light and dark themes. The use case restricts expensive regex backtracking before save."),
                    ))
                    .children(keyword_rows)
                    .child(div().text_size(px(12.)).child(crate::locale::text("添加关键词规则", "Add keyword rule")))
                    .child(div().text_size(theme.text_xs).text_color(theme.muted).child(crate::locale::text("匹配内容", "Pattern")))
                    .child(Input::new(&self.terminal_behavior_inputs.new_keyword_pattern).disabled(locked).w(px(520.)))
                    .child(div().text_size(theme.text_xs).text_color(theme.muted).child(crate::locale::text("备注", "Note")))
                    .child(Input::new(&self.terminal_behavior_inputs.new_keyword_note).disabled(locked).w(px(520.)))
                    .child(
                        Button::new("keyword-add")
                            .compact()
                            .primary()
                            .label(crate::locale::text("添加规则", "Add rule"))
                            .disabled(locked || !keywords.enabled)
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.add_keyword_rule(window, cx);
                            })),
                    ),
            );

        if !self.settings_input_errors.is_empty() {
            body = body.children(
                self.settings_input_errors
                    .values()
                    .map(|message| issue_row(message, theme)),
            );
        }
        body
    }
}

/// 通用枚举行只为有闭合类型选项的字段创建按钮，回调接收显式快照 setter。
fn enum_row<T: Copy + Debug + PartialEq + 'static>(
    label: impl Into<gpui_kit::SharedString>,
    current: T,
    options: &[(T, gpui_kit::SharedString)],
    disabled: bool,
    cx: &mut Context<PreferencesPanel>,
    update: fn(&mut SettingsSnapshot, T),
) -> impl IntoElement {
    let label = label.into();
    let buttons = options
        .iter()
        .cloned()
        .map(|(value, text)| {
            Button::new(format!("{}-{value:?}", label.as_ref()))
                .compact()
                .label(text)
                .disabled(disabled)
                .when(value == current, Button::primary)
                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                    panel.edit_settings(|settings| update(settings, value), cx);
                }))
        })
        .collect::<Vec<_>>();
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(190.)).text_size(px(12.)).child(label))
        .children(buttons)
}

/// 终端子设置输入固定宽度并继承父窗主题，不让数值控件挤压标签。
fn number_field(
    label: impl Into<gpui_kit::SharedString>,
    input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
    disabled: bool,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .child(div().w(px(220.)).text_size(px(12.)).child(label.into()))
        .child(Input::new(input).disabled(disabled).w(px(150.)))
}

/// 设置卡片统一使用全局主题令牌，系统主题切换不会留下硬编码色块。
fn section_card(title: impl Into<gpui_kit::SharedString>, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .bg(theme.panel)
        .rounded_md()
        .child(
            div()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title.into()),
        )
}

/// 建议来源开关逐项更新对应 provider，关闭建议时保留用户选择而不清空配置。
fn provider_switches(
    providers: &TerminalInlineSuggestionProviderSettings,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    div()
        .w_full()
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .bg(theme.sidebar)
        .rounded_md()
        .child(
            div()
                .text_size(px(12.))
                .child(crate::locale::text("建议来源", "Suggestion sources")),
        )
        .child(
            Switch::new("suggestion-provider-history")
                .checked(providers.history)
                .disabled(disabled)
                .label(crate::locale::text("本机命令历史", "Local command history"))
                .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| settings.terminal.inline_suggestion.providers.history = *enabled,
                        cx,
                    );
                })),
        )
        .child(
            Switch::new("suggestion-provider-remote-path")
                .checked(providers.remote_path)
                .disabled(disabled)
                .label(crate::locale::text("远端路径", "Remote paths"))
                .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| {
                            settings.terminal.inline_suggestion.providers.remote_path = *enabled
                        },
                        cx,
                    );
                })),
        )
        .child(
            Switch::new("suggestion-provider-remote-command")
                .checked(providers.remote_command)
                .disabled(disabled)
                .label(crate::locale::text("远端命令", "Remote commands"))
                .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| {
                            settings.terminal.inline_suggestion.providers.remote_command = *enabled
                        },
                        cx,
                    );
                })),
        )
        .child(
            Switch::new("suggestion-provider-git")
                .checked(providers.git)
                .disabled(disabled)
                .label(crate::locale::text(
                    "Git 分支和引用",
                    "Git branches and refs",
                ))
                .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| settings.terminal.inline_suggestion.providers.git = *enabled,
                        cx,
                    );
                })),
        )
        .child(
            Switch::new("suggestion-provider-spec")
                .checked(providers.spec)
                .disabled(disabled)
                .label(crate::locale::text(
                    "命令规范建议",
                    "Command specification suggestions",
                ))
                .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                    panel.edit_settings(
                        |settings| settings.terminal.inline_suggestion.providers.spec = *enabled,
                        cx,
                    );
                })),
        )
}

/// 关键词错误以语义色展示，便于用户在保存前定位无效规则。
fn issue_row(message: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(theme.error)
        .child(message.to_owned())
}

/// 每个规则的匹配模式可独立配置，保持原始顺序用于相互重叠的匹配结果。
fn keyword_mode_buttons(
    id: &str,
    current: TerminalKeywordHighlightMatchMode,
    disabled: bool,
    cx: &mut Context<PreferencesPanel>,
) -> Vec<Button> {
    [
        (
            TerminalKeywordHighlightMatchMode::Literal,
            crate::locale::text("字面文本", "Literal"),
        ),
        (
            TerminalKeywordHighlightMatchMode::WholeWord,
            crate::locale::text("整词", "Whole word"),
        ),
        (
            TerminalKeywordHighlightMatchMode::Regex,
            crate::locale::text("正则", "Regular expression"),
        ),
    ]
    .into_iter()
    .map(|(value, label)| {
        let rule_id = id.to_owned();
        Button::new(format!("keyword-mode-{id}-{value:?}"))
            .compact()
            .label(label)
            .disabled(disabled)
            .when(current == value, Button::primary)
            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                panel.update_keyword_rule(&rule_id, |rule| rule.match_mode = value, cx);
            }))
    })
    .collect()
}

/// 预设样式按钮保留现有 custom 状态，避免无颜色的错误 custom 配置悄悄覆盖颜色。
fn keyword_style_buttons(
    id: &str,
    current: TerminalKeywordHighlightStyle,
    disabled: bool,
    custom_colors: bool,
    cx: &mut Context<PreferencesPanel>,
) -> Vec<Button> {
    let mut options = vec![
        (
            TerminalKeywordHighlightStyle::Red,
            crate::locale::text("红", "Red"),
        ),
        (
            TerminalKeywordHighlightStyle::Orange,
            crate::locale::text("橙", "Orange"),
        ),
        (
            TerminalKeywordHighlightStyle::Yellow,
            crate::locale::text("黄", "Yellow"),
        ),
        (
            TerminalKeywordHighlightStyle::Green,
            crate::locale::text("绿", "Green"),
        ),
        (
            TerminalKeywordHighlightStyle::Cyan,
            crate::locale::text("青", "Cyan"),
        ),
        (
            TerminalKeywordHighlightStyle::Blue,
            crate::locale::text("蓝", "Blue"),
        ),
        (
            TerminalKeywordHighlightStyle::Purple,
            crate::locale::text("紫", "Purple"),
        ),
        (
            TerminalKeywordHighlightStyle::Pink,
            crate::locale::text("粉", "Pink"),
        ),
    ];
    if custom_colors || current == TerminalKeywordHighlightStyle::Custom {
        options.push((
            TerminalKeywordHighlightStyle::Custom,
            crate::locale::text("自定义色", "Custom"),
        ));
    }
    options
        .into_iter()
        .map(|(value, label)| {
            let rule_id = id.to_owned();
            Button::new(format!("keyword-style-{id}-{value:?}"))
                .compact()
                .label(label)
                .disabled(
                    disabled || (value == TerminalKeywordHighlightStyle::Custom && !custom_colors),
                )
                .when(current == value, Button::primary)
                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                    panel.update_keyword_rule(&rule_id, |rule| rule.style = value, cx);
                }))
        })
        .collect()
}

/// 终端调色板候选使用 application 当前支持的四个稳定预设。
fn scheme_options() -> [(TerminalColorScheme, gpui_kit::SharedString); 4] {
    [
        (TerminalColorScheme::Kerminal, "Kerminal".into()),
        (TerminalColorScheme::TokyoNight, "Tokyo Night".into()),
        (TerminalColorScheme::Solarized, "Solarized".into()),
        (TerminalColorScheme::Github, "GitHub".into()),
    ]
}

/// 终端字重值写回完整 snapshot，不触碰正在运行的会话对象。
fn set_terminal_font_weight(settings: &mut SettingsSnapshot, value: TerminalFontWeight) {
    settings.terminal.font_weight = value;
}

/// 右键行为保持互斥枚举，防止同时配置菜单和粘贴造成歧义。
fn set_terminal_right_click(settings: &mut SettingsSnapshot, value: TerminalRightClickBehavior) {
    settings.terminal.right_click_behavior = value;
}

/// 浅色终端主题使用单独 palette 字段，避免切换桌面主题时覆盖暗色偏好。
fn set_light_color_scheme(settings: &mut SettingsSnapshot, value: TerminalColorScheme) {
    settings.terminal.light_color_scheme = value;
}

/// 深色终端主题使用单独 palette 字段，保留用户为浅色界面设定的配色。
fn set_dark_color_scheme(settings: &mut SettingsSnapshot, value: TerminalColorScheme) {
    settings.terminal.dark_color_scheme = value;
}

/// 控制命令建议呈现状态并同步启用字段，application 仍负责最终 normalization。
fn set_suggestion_presentation(
    settings: &mut SettingsSnapshot,
    value: TerminalCommandSuggestionPresentation,
) {
    settings.terminal.inline_suggestion.presentation = value;
    settings.terminal.inline_suggestion.enabled =
        value != TerminalCommandSuggestionPresentation::Off;
}

/// 接受键与显示方式分别持久化，切换快捷键不会隐式开启已关闭的建议功能。
fn set_suggestion_accept_key(
    settings: &mut SettingsSnapshot,
    value: TerminalInlineSuggestionAcceptKey,
) {
    settings.terminal.inline_suggestion.accept_key = value;
}

/// 远端刷新仅开放关闭与安全刷新，UI 不提供未经安全审查的任意探测模式。
fn set_remote_refresh(
    settings: &mut SettingsSnapshot,
    value: TerminalCommandSuggestionRemoteRefresh,
) {
    settings.terminal.inline_suggestion.remote_refresh = value;
    settings.terminal.inline_suggestion.remote_probe_enabled =
        value != TerminalCommandSuggestionRemoteRefresh::Off;
}
