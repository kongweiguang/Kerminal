//! 终端建议与关键词规则的 typed 草稿编辑。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::preferences::{
    TerminalCommandSuggestionPresentation, TerminalKeywordHighlightRule,
};

use super::forms::set_input;
use super::{KeywordTextField, PreferencesPanel};

impl PreferencesPanel {
    /// 按配置规则 ID 重建动态输入实体，确保重载后旧行的 Change 事件不再更新新快照。
    pub(super) fn rebuild_keyword_inputs(
        &mut self,
        rules: &[TerminalKeywordHighlightRule],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.keyword_rule_subscriptions.clear();
        self.terminal_behavior_inputs.keyword_rules.clear();
        for rule in rules {
            let (inputs, subscriptions) = Self::create_keyword_rule_inputs(
                rule.id.clone(),
                &rule.pattern,
                &rule.note,
                rule.custom_colors.as_ref(),
                window,
                cx,
            );
            self.terminal_behavior_inputs.keyword_rules.push(inputs);
            self.keyword_rule_subscriptions.extend(subscriptions);
        }
    }

    /// 将新建字段暂存到本地草稿，已有规则字段按稳定 ID 写入 settings 快照。
    pub(super) fn update_keyword_text(
        &mut self,
        field: KeywordTextField,
        value: String,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        match field {
            KeywordTextField::NewPattern => self.new_keyword_pattern = value,
            KeywordTextField::NewNote => self.new_keyword_note = value,
            KeywordTextField::RulePattern(id) => self.edit_settings(
                |settings| {
                    if let Some(rule) = settings
                        .terminal
                        .keyword_highlights
                        .rules
                        .iter_mut()
                        .find(|rule| rule.id == id)
                    {
                        rule.pattern = value;
                    }
                },
                cx,
            ),
            KeywordTextField::RuleNote(id) => self.edit_settings(
                |settings| {
                    if let Some(rule) = settings
                        .terminal
                        .keyword_highlights
                        .rules
                        .iter_mut()
                        .find(|rule| rule.id == id)
                    {
                        rule.note = value;
                    }
                },
                cx,
            ),
            KeywordTextField::LightForeground(id) => {
                let color = optional_color(value);
                self.update_keyword_rule(
                    &id,
                    move |rule| {
                        rule.custom_colors
                            .get_or_insert_with(Default::default)
                            .light
                            .foreground = color;
                    },
                    cx,
                );
            }
            KeywordTextField::LightBackground(id) => {
                let color = optional_color(value);
                self.update_keyword_rule(
                    &id,
                    move |rule| {
                        rule.custom_colors
                            .get_or_insert_with(Default::default)
                            .light
                            .background = color;
                    },
                    cx,
                );
            }
            KeywordTextField::DarkForeground(id) => {
                let color = optional_color(value);
                self.update_keyword_rule(
                    &id,
                    move |rule| {
                        rule.custom_colors
                            .get_or_insert_with(Default::default)
                            .dark
                            .foreground = color;
                    },
                    cx,
                );
            }
            KeywordTextField::DarkBackground(id) => {
                let color = optional_color(value);
                self.update_keyword_rule(
                    &id,
                    move |rule| {
                        rule.custom_colors
                            .get_or_insert_with(Default::default)
                            .dark
                            .background = color;
                    },
                    cx,
                );
            }
        }
        self.settings_input_errors.remove("keyword_rules");
        self.refresh_settings_issues();
        cx.notify();
    }

    /// 使用 application 规则构造器生成 ID，并在无效输入时保留用户编辑内容。
    pub(in crate::features::preferences) fn add_keyword_rule(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let pattern = self.new_keyword_pattern.trim();
        let note = self.new_keyword_note.trim();
        let Some(snapshot) = self.settings.as_ref().map(|draft| draft.value()) else {
            return;
        };
        if pattern.is_empty() || pattern.chars().count() > 256 || note.chars().count() > 160 {
            self.settings_input_errors.insert(
                "keyword_rules",
                crate::locale::text(
                    "关键词不能为空；关键词最多 256 个字符，备注最多 160 个字符。",
                    "A keyword is required and must be at most 256 characters; notes must be at most 160 characters.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if snapshot.terminal.keyword_highlights.rules.len() >= 64 {
            self.settings_input_errors.insert(
                "keyword_rules",
                crate::locale::text(
                    "关键词规则最多保存 64 条。",
                    "At most 64 keyword rules can be saved.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let rule = TerminalKeywordHighlightRule::new(pattern.to_owned(), note.to_owned());
        self.edit_settings(
            |settings| settings.terminal.keyword_highlights.rules.push(rule),
            cx,
        );
        self.new_keyword_pattern.clear();
        self.new_keyword_note.clear();
        set_input(
            &self.terminal_behavior_inputs.new_keyword_pattern,
            String::new(),
            window,
            cx,
        );
        set_input(
            &self.terminal_behavior_inputs.new_keyword_note,
            String::new(),
            window,
            cx,
        );
        self.settings_input_errors.remove("keyword_rules");
        if let Some(rules) = self
            .settings
            .as_ref()
            .map(|draft| draft.value().terminal.keyword_highlights.rules.clone())
        {
            self.rebuild_keyword_inputs(&rules, window, cx);
        }
    }

    /// 通过 rule ID 变更开关、匹配模式、样式和大小写策略，数组展示顺序保持原样。
    pub(in crate::features::preferences) fn update_keyword_rule(
        &mut self,
        id: &str,
        update: impl FnOnce(&mut TerminalKeywordHighlightRule),
        cx: &mut Context<Self>,
    ) {
        self.edit_settings(
            |settings| {
                if let Some(rule) = settings
                    .terminal
                    .keyword_highlights
                    .rules
                    .iter_mut()
                    .find(|rule| rule.id == id)
                {
                    update(rule);
                }
            },
            cx,
        );
    }

    /// 删除规则后重建剩余输入，避免被移除的实体仍持有可编辑的旧 ID。
    pub(in crate::features::preferences) fn delete_keyword_rule(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_settings(
            |settings| {
                settings
                    .terminal
                    .keyword_highlights
                    .rules
                    .retain(|rule| rule.id != id);
            },
            cx,
        );
        if let Some(rules) = self
            .settings
            .as_ref()
            .map(|draft| draft.value().terminal.keyword_highlights.rules.clone())
        {
            self.rebuild_keyword_inputs(&rules, window, cx);
        }
    }

    /// 关闭建议时保留其来源偏好，重新启用后可恢复用户已有 provider 选择。
    pub(in crate::features::preferences) fn set_suggestion_enabled(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        self.edit_settings(
            |settings| {
                settings.terminal.inline_suggestion.enabled = enabled;
                if !enabled {
                    settings.terminal.inline_suggestion.presentation =
                        TerminalCommandSuggestionPresentation::Off;
                } else if settings.terminal.inline_suggestion.presentation
                    == TerminalCommandSuggestionPresentation::Off
                {
                    settings.terminal.inline_suggestion.presentation =
                        TerminalCommandSuggestionPresentation::InlineAndMenu;
                }
            },
            cx,
        );
    }
}

/// 颜色输入空值表示清除覆盖，其余原样保留供 application 做严格 #RRGGBB 校验。
fn optional_color(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}
