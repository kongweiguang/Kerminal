//! 关键词高亮规则列表与单规则编辑器。
//!
//! @author kongweiguang

use std::{cell::Cell, rc::Rc};

use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, IntoElement, Pixels, Render, ScrollHandle,
    ScrollWheelEvent, Window,
    base::ElementExt as _,
    component::{
        ActiveTheme, Disableable,
        button::{Button, ButtonVariants},
        input::Textarea,
        scroll::ScrollableElement,
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::{
    TerminalKeywordHighlightMatchMode, TerminalKeywordHighlightRule, TerminalKeywordHighlightStyle,
};

use crate::theme::ThemeTokens;

use super::{
    panel::PreferencesPanel,
    panel::terminal::{KEYWORD_RULE_LIMIT, parse_hex_color},
};

/// main 的八个色板值分别提供浅深主题预览，列表顺序与持久规则顺序一致。
///
/// @author kongweiguang
const PRESET_STYLES: [TerminalKeywordHighlightStyle; 8] = [
    TerminalKeywordHighlightStyle::Red,
    TerminalKeywordHighlightStyle::Orange,
    TerminalKeywordHighlightStyle::Yellow,
    TerminalKeywordHighlightStyle::Green,
    TerminalKeywordHighlightStyle::Cyan,
    TerminalKeywordHighlightStyle::Blue,
    TerminalKeywordHighlightStyle::Purple,
    TerminalKeywordHighlightStyle::Pink,
];

#[derive(Clone)]
struct KeywordRuleDrag {
    id: String,
    pattern: String,
}

/// Editor status flags travel together because they jointly control mutability and validation feedback.
struct KeywordRuleEditorState {
    is_new: bool,
    pending: bool,
    error: Option<String>,
    locked: bool,
}

struct KeywordRuleDragPreview {
    pattern: String,
}

/// GPUI 先更新子区偏移再派发此监听器，行单位必须采用子区绘制时的行高还原原偏移。
/// 仅在子区原本还能沿该方向滚动时阻止外层同步滚动。
/// 分数布局中的 f32 偏移还原存在舍入误差，边界保留千分之一逻辑像素余量。
/// 已到边界则让滚轮自然传给正文，避免用户必须把指针移出列表或编辑区才能继续浏览。
///
/// @author kongweiguang
fn route_keyword_scroll(
    event: &ScrollWheelEvent,
    scroll: &ScrollHandle,
    line_height: Pixels,
    cx: &mut App,
) {
    let delta = event.delta.pixel_delta(line_height);
    let vertical = if delta.y != px(0.) { delta.y } else { delta.x };
    let before = scroll.offset().y - vertical;
    let max = scroll.max_offset().y;
    let edge_tolerance = px(0.001);
    if (vertical < px(0.) && before > -max + edge_tolerance)
        || (vertical > px(0.) && before < -edge_tolerance)
    {
        cx.stop_propagation();
    }
}

impl Render for KeywordRuleDragPreview {
    /// 拖动时显示被移动的规则文本，避免只有空占位框而无法辨认来源。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .bg(cx.theme().popover)
            .text_color(cx.theme().foreground)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .child(self.pattern.clone())
    }
}

impl PreferencesPanel {
    /// 使用实际设置内容宽度切换主从布局，两侧独立滚动并保留固定操作区。
    /// 窄窗口为列表和编辑器保留最小空间，超出正文的部分由外层滚动而非压扁输入框。
    /// 列表行滚轮使用预绘时的实际行高，子区域未到边界时不带动外层。
    /// 未保存草稿按规则身份保留，数量达到上限时仍允许恢复但不能新增提交。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn render_keyword_highlights(
        &mut self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return div()
                .id("keyword-highlights-loading")
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "正在读取关键词高亮设置…",
                    "Loading keyword highlight settings…",
                ));
        };

        let wide = self.keyword_editor.wide_layout.unwrap_or(false);
        let panel = cx.weak_entity();
        window.on_next_frame(move |_, app| {
            let _ = panel.update(app, |panel, cx| {
                let width = panel.content_scroll.bounds().size.width;
                if width > px(0.) {
                    let measured_wide = width >= px(680.);
                    if panel.keyword_editor.wide_layout != Some(measured_wide) {
                        panel.keyword_editor.wide_layout = Some(measured_wide);
                        cx.notify();
                    }
                }
            });
        });
        let keyword_settings = settings.terminal.keyword_highlights;
        let locked = self.settings_controls_locked();
        let query = self.keyword_editor.query.trim().to_lowercase();
        let session = self.keyword_editor.session.as_ref();
        let selected_rule_id = session.map(|session| session.draft.id.clone());
        let is_new = session.is_some_and(|session| session.is_new);
        let draft = session.map(|session| session.draft.clone());
        let pending = self.keyword_editor.save_pending.is_some();
        let error = self.keyword_editor.error.clone();
        let undo_receipt = self.keyword_editor.undo_receipt.as_ref().map(|receipt| {
            (
                receipt.rule.pattern.clone(),
                receipt.expires_at > std::time::Instant::now(),
            )
        });

        let visible_rules = keyword_settings
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| {
                query.is_empty()
                    || [
                        rule.pattern.as_str(),
                        rule.note.as_str(),
                        match_mode_label(rule.match_mode),
                    ]
                    .join(" ")
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, rule)| {
                self.render_keyword_rule_row(
                    rule,
                    index,
                    keyword_settings.rules.len(),
                    selected_rule_id.as_deref() == Some(rule.id.as_str()),
                    theme,
                    cx,
                )
                .into_any_element()
            })
            .collect::<Vec<_>>();

        let mut root = div()
            .id("settings-keyword-highlights-panel")
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
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .size(px(36.))
                                    .flex_shrink_0()
                                    .rounded(theme.control_radius)
                                    .bg(theme.surface_selected())
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Highlighter)
                                        .size(px(16.)).text_color(theme.settings_choice_foreground(true))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .text_size(px(16.))
                                        .child(crate::locale::text("关键词高亮", "Keyword highlights")))
                                    .child(div().text_size(theme.text_xs).text_color(theme.muted)
                                        .child(crate::locale::text(
                                            "全局匹配普通终端可见文本；靠上的规则优先。",
                                            "Rules match visible text in regular terminals; earlier rules have priority.",
                                        ))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Switch::new("keyword-highlights-enabled")
                                    .track_size(px(40.), px(24.))
                                    .label_side(gpui_kit::component::Side::Left)
                                    .checked(keyword_settings.enabled)
                                    .disabled(locked)
                                    .label(crate::locale::text("功能总开关", "Enable highlights"))
                                    .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                        panel.edit_settings(|settings| {
                                            settings.terminal.keyword_highlights.enabled = *enabled;
                                        }, cx);
                                    })),
                            )
                            .child(
                                Button::new("keyword-new")
                                    .primary()
                                    .icon(gpui_kit::assets::IconName::Plus)
                                    .label(if self.keyword_editor.has_parked_new() {
                                        crate::locale::text("继续新建", "Resume new rule")
                                    } else {
                                        crate::locale::text("新建规则", "New rule")
                                    })
                                    .disabled(locked || is_new || (keyword_settings.rules.len() >= KEYWORD_RULE_LIMIT && !self.keyword_editor.has_parked_new()))
                                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                        panel.start_keyword_rule(window, cx);
                                    })),
                            ),
                    ),
            );

        if let Some((pattern, _available)) = undo_receipt.filter(|(_, available)| *available) {
            root = root.child(
                div()
                    .id("keyword-delete-undo-status")
                    .w_full()
                    .min_h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .bg(theme.sidebar)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        format!("规则“{pattern}”已删除，可在 15 秒内撤销。"),
                        format!("Rule “{pattern}” was deleted. Undo is available for 15 seconds."),
                    ))
                    .child(
                        Button::new("keyword-undo-delete")
                            .custom(super::render_settings::settings_ghost_variant(theme, cx))
                            .icon(gpui_kit::assets::IconName::Undo2)
                            .label(crate::locale::text("撤销", "Undo"))
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.undo_keyword_rule_delete(window, cx);
                            })),
                    ),
            );
        }

        if keyword_settings.rules.is_empty() && !is_new {
            root = root.child(
                div()
                    .id("keyword-empty-state")
                    .w_full()
                    .min_h(px(256.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .p_6()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(div().size(px(48.)).rounded_xl().bg(theme.muted_surface())
                        .flex().items_center().justify_center()
                        .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Highlighter)
                            .size(px(20.)).text_color(theme.muted)))
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(crate::locale::text("还没有高亮规则", "No highlight rules yet")),
                    )
                    .child(
                        div()
                            .max_w(px(420.))
                            .text_align(gpui_kit::TextAlign::Center)
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "创建第一条规则后，当前和之后打开的普通终端都会自动应用。",
                                "Create a rule to apply it to current and future regular terminals.",
                            )),
                    )
                    .child(
                        Button::new("keyword-empty-new")
                            .primary()
                            .icon(gpui_kit::assets::IconName::Plus)
                            .label(crate::locale::text("新建规则", "New rule"))
                            .disabled(locked)
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.start_keyword_rule(window, cx);
                            })),
                    ),
            );
        } else {
            let mut list = div()
                .id("keyword-rule-list")
                .min_h(px(0.))
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(crate::locale::text("规则列表", "Rules")),
                                )
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(crate::locale::text(
                                            format!(
                                                "已创建 {} 条，最多 64 条",
                                                keyword_settings.rules.len()
                                            ),
                                            format!(
                                                "{} created, maximum 64",
                                                keyword_settings.rules.len()
                                            ),
                                        )),
                                ),
                        )
                        .when(!keyword_settings.enabled, |row| {
                            row.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(crate::locale::text("已全局暂停", "Paused globally")),
                            )
                        }),
                )
                .child(
                    div().id("keyword-search-field").child(
                        super::render_settings::settings_input(
                            &self.keyword_editor.search_input,
                            theme,
                        )
                        .accessibility_id("keyword-search")
                        .disabled(locked)
                        .w_full(),
                    ),
                );
            let list_scroll = self.keyword_editor.list_scroll.clone();
            let list_line_height = Rc::new(Cell::new(px(0.)));
            let list_prepaint_line_height = list_line_height.clone();
            let mut list_content = div()
                .id("keyword-rule-list-scroll")
                .debug_selector(|| "keyword-rule-list-viewport".into())
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .gap_2()
                .overflow_y_scroll()
                .track_scroll(&self.keyword_editor.list_scroll)
                .on_prepaint(move |_, window, _| {
                    list_prepaint_line_height.set(window.line_height());
                })
                .on_scroll_wheel(move |event, _, cx| {
                    route_keyword_scroll(event, &list_scroll, list_line_height.get(), cx);
                })
                .vertical_scrollbar(&self.keyword_editor.list_scroll);
            for row in visible_rules {
                list_content = list_content.child(row);
            }
            if !query.is_empty()
                && !keyword_settings.rules.iter().any(|rule| {
                    [
                        rule.pattern.as_str(),
                        rule.note.as_str(),
                        match_mode_label(rule.match_mode),
                    ]
                    .join(" ")
                    .to_lowercase()
                    .contains(&query)
                })
            {
                list_content = list_content.child(
                    div()
                        .id("keyword-no-results")
                        .w_full()
                        .p_6()
                        .text_align(gpui_kit::TextAlign::Center)
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "没有匹配的规则。",
                            "No matching rules.",
                        )),
                );
            }
            list = list.child(list_content);

            root = root.child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .when(!wide, |row| row.flex_col().min_h(px(480.)))
                    .items_stretch()
                    .gap_4()
                    .child(
                        list.when(wide, |list| {
                            list.flex_1()
                                .flex_grow(0.82)
                                .flex_basis(px(0.))
                                .min_w(px(0.))
                                .h_full()
                        })
                        .when(!wide, |list| {
                            list.w_full().h(px(220.)).flex_shrink_0().min_w(px(0.))
                        }),
                    )
                    .child(
                        div()
                            .when(wide, |editor| {
                                editor
                                    .flex_1()
                                    .flex_grow(1.18)
                                    .flex_basis(px(0.))
                                    .min_w(px(0.))
                                    .h_full()
                            })
                            .when(!wide, |editor| {
                                editor.w_full().flex_1().min_h(px(240.)).min_w(px(0.))
                            })
                            .child(self.render_keyword_rule_editor(
                                draft.as_ref(),
                                KeywordRuleEditorState {
                                    is_new,
                                    pending,
                                    error,
                                    locked,
                                },
                                theme,
                                cx,
                            )),
                    ),
            );
        }
        root.child(self.render_save_notice(theme))
    }

    /// 列表条目只操作持久规则；编辑器草稿字段仅通过其独立保存按钮提交。
    /// 两行摘要按内容撑高，列表滚动时不压缩行高，避免单行按钮默认高度裁掉模式和备注。
    ///
    /// @author kongweiguang
    fn render_keyword_rule_row(
        &mut self,
        rule: &TerminalKeywordHighlightRule,
        index: usize,
        count: usize,
        selected: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mutation_locked =
            self.settings_controls_locked() || self.keyword_editor.save_pending.is_some();
        let id = rule.id.clone();
        let selected_id = id.clone();
        let enabled_id = id.clone();
        let move_up_id = id.clone();
        let move_down_id = id.clone();
        let target_id = id.clone();
        let drag_target_id = target_id.clone();
        let drag_id = id.clone();
        let drag_pattern = rule.pattern.clone();
        let swatches = rule_swatch_colors(rule);
        let drag_payload = KeywordRuleDrag {
            id: drag_id,
            pattern: drag_pattern,
        };

        div()
            .id(format!("keyword-rule-row-{id}"))
            .w_full()
            .min_h(px(64.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_md()
            .bg(if selected {
                theme.surface_selected()
            } else {
                theme.panel
            })
            .border_1()
            .border_color(if selected {
                theme.active_connection_indicator().opacity(0.45)
            } else {
                theme.border.opacity(0.)
            })
            .child(
                div()
                    .id(format!("keyword-rule-drag-{id}"))
                    .flex_none()
                    .px_1()
                    .text_color(theme.muted)
                    .when(!mutation_locked, |handle| {
                        handle.on_drag(drag_payload, move |dragged, _, _, cx| {
                            cx.new(|_| KeywordRuleDragPreview {
                                pattern: dragged.pattern.clone(),
                            })
                        })
                    })
                    .child("⋮⋮"),
            )
            .child(
                Button::new(format!("keyword-rule-{id}"))
                    .custom(super::render_settings::settings_ghost_variant(theme, cx))
                    .flex_1()
                    .min_w(px(0.))
                    .h_auto()
                    .py_2()
                    .tooltip(if rule.note.is_empty() {
                        rule.pattern.clone()
                    } else {
                        format!("{}\n{}", rule.pattern, rule.note)
                    })
                    .accessibility_label(crate::locale::text(
                        format!("编辑规则 {}", rule.pattern),
                        format!("Edit rule {}", rule.pattern),
                    ))
                    .child(
                        div()
                            .debug_selector({
                                let id = id.clone();
                                move || format!("keyword-rule-summary-{id}")
                            })
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_1()
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(color_pair_swatch(swatches.0.0, swatches.0.1))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .text_size(theme.text_sm)
                                            .child(rule.pattern.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(
                                        div()
                                            .px_1()
                                            .bg(theme.sidebar)
                                            .rounded_sm()
                                            .child(match_mode_label(rule.match_mode)),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .child(if rule.note.is_empty() {
                                                crate::locale::text("无备注", "No note")
                                            } else {
                                                rule.note.clone().into()
                                            }),
                                    ),
                            ),
                    )
                    .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                        panel.select_keyword_rule(&selected_id, window, cx);
                    })),
            )
            .child(
                Button::new(format!("keyword-rule-up-{id}"))
                    .compact()
                    .custom(super::render_settings::settings_ghost_variant(theme, cx))
                    .label("↑")
                    .accessibility_label(crate::locale::text(
                        format!("上移规则 {}", rule.pattern),
                        format!("Move rule {} up", rule.pattern),
                    ))
                    .tooltip(crate::locale::text("上移", "Move up"))
                    .disabled(mutation_locked || index == 0)
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.move_keyword_rule(&move_up_id, -1, cx);
                    })),
            )
            .child(
                Button::new(format!("keyword-rule-down-{id}"))
                    .compact()
                    .custom(super::render_settings::settings_ghost_variant(theme, cx))
                    .label("↓")
                    .accessibility_label(crate::locale::text(
                        format!("下移规则 {}", rule.pattern),
                        format!("Move rule {} down", rule.pattern),
                    ))
                    .tooltip(crate::locale::text("下移", "Move down"))
                    .disabled(mutation_locked || index + 1 >= count)
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.move_keyword_rule(&move_down_id, 1, cx);
                    })),
            )
            .child(
                Switch::new(format!("keyword-rule-enabled-{id}"))
                    .track_size(px(40.), px(24.))
                    .checked(rule.enabled)
                    .disabled(mutation_locked)
                    .accessibility_label(crate::locale::text(
                        format!("启用规则 {}", rule.pattern),
                        format!("Enable rule {}", rule.pattern),
                    ))
                    .on_change(cx.listener(move |panel, enabled: &bool, _, cx| {
                        panel.toggle_keyword_rule(&enabled_id, *enabled, cx);
                    })),
            )
            .drag_over::<KeywordRuleDrag>(move |style, dragged, _, _| {
                if dragged.id == drag_target_id {
                    style
                } else {
                    style.border_l_2().border_color(theme.accent)
                }
            })
            .on_drop(cx.listener(move |panel, dragged: &KeywordRuleDrag, _, cx| {
                panel.move_keyword_rule_before(&dragged.id, &target_id, cx);
            }))
    }

    /// 编辑字段独立滚动，固定底部操作区保证窄窗和自定义色板展开时仍可保存。
    /// 行滚轮按预绘行高还原真实偏移，避免样式继承导致错误的边界判断。
    /// 等待 CAS 时锁定编辑字段，避免在回执确认前改变同一提交会话。
    ///
    /// @author kongweiguang
    fn render_keyword_rule_editor(
        &mut self,
        draft: Option<&TerminalKeywordHighlightRule>,
        state: KeywordRuleEditorState,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let KeywordRuleEditorState {
            is_new,
            pending,
            error,
            locked,
        } = state;
        let Some(draft) = draft else {
            return div()
                .id("keyword-rule-editor-empty")
                .w_full()
                .h_full()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "选择一条规则进行编辑。",
                    "Select a rule to edit.",
                ));
        };

        let draft = draft.clone();
        let custom_enabled = draft.style == TerminalKeywordHighlightStyle::Custom;
        let foreground = preview_colors(&draft).0;
        let background = preview_colors(&draft).1;
        let terminal = self
            .settings
            .as_ref()
            .map(|settings| settings.value().terminal.clone())
            .unwrap_or_default();
        let dark = cx.theme().is_dark();
        let terminal_colors =
            super::render_terminal::terminal_preview_colors(terminal.color_scheme, dark);
        let preview_font = crate::terminal_metrics::TerminalMetrics::from_settings(
            &terminal,
            &cx.text_system().all_font_names(),
        )
        .font_family;
        let pattern_count = draft.pattern.chars().count();
        let note_count = draft.note.chars().count();

        let editor_scroll = self.keyword_editor.editor_scroll.clone();
        let editor_line_height = Rc::new(Cell::new(px(0.)));
        let editor_prepaint_line_height = editor_line_height.clone();
        let mut editor = div()
            .id("keyword-rule-editor-content")
            .debug_selector(|| "keyword-rule-editor-viewport".into())
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .overflow_y_scroll()
            .track_scroll(&self.keyword_editor.editor_scroll)
            .on_prepaint(move |_, window, _| {
                editor_prepaint_line_height.set(window.line_height());
            })
            .on_scroll_wheel(move |event, _, cx| {
                route_keyword_scroll(event, &editor_scroll, editor_line_height.get(), cx);
            })
            .vertical_scrollbar(&self.keyword_editor.editor_scroll)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(if is_new {
                                        crate::locale::text("新建规则", "New rule")
                                    } else {
                                        crate::locale::text("编辑规则", "Edit rule")
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(crate::locale::text(
                                        "保存成功后立即应用到所有普通终端；全屏 TUI 不参与高亮。",
                                        "Saved rules apply to regular terminals; full-screen TUI apps are excluded.",
                                    )),
                            ),
                    )
                    .child(
                        Switch::new("keyword-rule-enabled-draft")
                            .track_size(px(40.), px(24.))
                            .label_side(gpui_kit::component::Side::Left)
                            .flex_shrink_0()
                            .checked(draft.enabled)
                            .disabled(locked || pending)
                            .label(crate::locale::text("启用规则", "Enable rule"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.update_keyword_rule_draft(|rule| rule.enabled = *enabled, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text("关键词或表达式", "Keyword or expression"))
                            .child(format!("{pattern_count} / 256")),
                    )
                    .child(
                        div()
                            .id("keyword-pattern-field")
                            .debug_selector(|| "keyword-pattern-field".into())
                            .child(
                                super::render_settings::settings_input(&self.keyword_editor.pattern_input, theme)
                                    .accessibility_id("keyword-pattern")
                                    .disabled(locked || pending)
                                    .w_full(),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text("匹配模式", "Match mode")),
                    )
                    .child(
                        div()
                            .id("keyword-match-mode")
                            .w_full()
                            .flex()
                            .gap_1()
                            .p_1()
                            .bg(theme.sidebar)
                            .border_1()
                            .border_color(theme.border)
                            .rounded_md()
                            .children([
                                mode_button(TerminalKeywordHighlightMatchMode::Literal, "keyword-mode-Literal", crate::locale::text("文本", "Literal"), draft.match_mode, locked || pending, theme, cx),
                                mode_button(TerminalKeywordHighlightMatchMode::WholeWord, "keyword-mode-WholeWord", crate::locale::text("整词", "Whole word"), draft.match_mode, locked || pending, theme, cx),
                                mode_button(TerminalKeywordHighlightMatchMode::Regex, "keyword-mode-Regex", crate::locale::text("正则", "Regex"), draft.match_mode, locked || pending, theme, cx),
                            ]),
                    ),
            )
            .child(
                div()
                    .id("keyword-case-sensitive-row")
                    .w_full()
                    .min_h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .rounded(theme.control_radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.field_surface())
                    .child(div().flex_1().min_w(px(0.)).text_size(theme.text_xs)
                        .child(div().text_color(theme.foreground).child(crate::locale::text("区分大小写", "Case sensitive")))
                        .child(div().text_color(theme.muted).child(crate::locale::text("关闭时使用 Unicode 大小写匹配", "Use Unicode case matching when disabled"))))
                    .child(Switch::new("keyword-case-sensitive")
                    .track_size(px(40.), px(24.))
                    .checked(draft.case_sensitive)
                    .disabled(locked || pending)
                    .accessibility_label(crate::locale::text("区分关键词大小写", "Case sensitive keyword matching"))
                    .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                        cx.stop_propagation();
                        panel.update_keyword_rule_draft(|rule| rule.case_sensitive = *enabled, cx);
                    })))
                    .when(!(locked || pending), |row| row.cursor_pointer().on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.update_keyword_rule_draft(|rule| rule.case_sensitive = !rule.case_sensitive, cx);
                    }))),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text("备注", "Note"))
                            .child(format!("{note_count} / 160")),
                    )
                    .child(
                        div()
                            .id("keyword-note-field")
                            .debug_selector(|| "keyword-note-field".into())
                            .child(
                                Textarea::new(&self.keyword_editor.note_input)
                                    .bg(theme.field_surface())
                                    .border_color(theme.strong_border())
                                    .rounded(theme.control_radius)
                                    .accessibility_id("keyword-note")
                                    .disabled(locked || pending)
                                    .h(px(56.))
                                    .w_full(),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text("自适应色板", "Adaptive palette")),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .children(PRESET_STYLES.into_iter().map(|style| {
                                let selected = draft.style == style;
                                let (_, light_bg, _, dark_bg) = style_palette(style);
                                let background = if cx.theme().is_dark() { dark_bg } else { light_bg };
                                Button::new(format!("keyword-style-{style:?}"))
                                    .compact()
                                    .size(px(36.))
                                    .p(px(6.))
                                    .outline()
                                    .border_color(if selected { theme.accent } else { theme.border })
                                    .tooltip(style_label(style))
                                    .accessibility_label(crate::locale::text(
                                        format!("使用{}高亮色板", style_label(style)),
                                        format!("Use {} highlight palette", style_label(style)),
                                    ))
                                    .disabled(locked || pending)
                                    .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                        panel.set_keyword_rule_style(style, window, cx);
                                    }))
                                    .child(div().size_full().rounded_md().border_1().border_color(theme.border).bg(parse_hex_color(background).unwrap()))
                            }))
                            .child(
                                Button::new("keyword-style-Custom")
                                    .compact()
                                    .h(px(36.))
                                    .outline()
                                    .label(crate::locale::text("自定义", "Custom"))
                                    .disabled(locked || pending)
                                    .text_color(theme.settings_choice_foreground(custom_enabled))
                                    .when(custom_enabled, |button| button.border_color(theme.accent).bg(theme.surface_selected()))
                                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                        panel.set_keyword_rule_style(TerminalKeywordHighlightStyle::Custom, window, cx);
                                    })),
                            ),
                    ),
            );

        if custom_enabled {
            let groups = [true, false].into_iter().map(|light| {
                let title = if light {
                    crate::locale::text("浅色主题", "Light theme")
                } else {
                    crate::locale::text("深色主题", "Dark theme")
                };
                let rows = self
                    .keyword_editor
                    .colors
                    .iter()
                    .filter(|field| {
                        matches!(
                            field.target,
                            super::panel::terminal::KeywordColorTarget::LightForeground
                                | super::panel::terminal::KeywordColorTarget::LightBackground
                        ) == light
                    })
                    .map(|field| {
                        let target = field.target;
                        let id = target.id_part();
                        let foreground = matches!(
                            target,
                            super::panel::terminal::KeywordColorTarget::LightForeground
                                | super::panel::terminal::KeywordColorTarget::DarkForeground
                        );
                        let label = if foreground {
                            crate::locale::text("文字", "Text")
                        } else {
                            crate::locale::text("背景", "Background")
                        };
                        let color_label = crate::locale::text(
                            format!("{title}{label}颜色"),
                            format!("{title} {label} color"),
                        );
                        let picker_id = format!("keyword-color-picker-{id}");
                        let selector = picker_id.clone();
                        let input_id = format!("keyword-color-field-{id}");
                        let input_selector = input_id.clone();
                        let picker =
                            if locked || pending {
                                Button::new(format!("keyword-color-disabled-{id}"))
                                    .outline()
                                    .disabled(true)
                                    .w(px(32.))
                                    .h(px(32.))
                                    .accessibility_label(color_label.clone())
                                    .child(div().size(px(20.)).rounded_sm().bg(
                                        field.picker.read(cx).value().unwrap_or(theme.foreground),
                                    ))
                                    .into_any_element()
                            } else {
                                field.picker_view.clone().into_any_element()
                            };
                        div()
                            .id(format!("keyword-color-row-{id}"))
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(52.))
                                    .flex_shrink_0()
                                    .text_size(theme.text_xs)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .id(picker_id)
                                    .debug_selector(move || selector.clone())
                                    .flex_shrink_0()
                                    .child(picker),
                            )
                            .child(
                                div()
                                    .id(input_id)
                                    .debug_selector(move || input_selector.clone())
                                    .min_w(px(0.))
                                    .flex_1()
                                    .child(
                                        super::render_settings::settings_input(&field.input, theme)
                                            .accessibility_id(format!("keyword-color-{id}"))
                                            .disabled(locked || pending)
                                            .text_size(px(12.))
                                            .px_2()
                                            .w_full(),
                                    ),
                            )
                            .child(
                                Button::new(format!("keyword-color-reset-{id}"))
                                    .compact()
                                    .custom(super::render_settings::settings_ghost_variant(
                                        theme, cx,
                                    ))
                                    .w(px(32.))
                                    .h(px(32.))
                                    .icon(gpui_kit::assets::IconName::RotateCcw)
                                    .accessibility_label(crate::locale::text(
                                        format!("重置{color_label}"),
                                        format!("Reset {color_label}"),
                                    ))
                                    .tooltip(crate::locale::text(
                                        "跟随终端原色",
                                        "Use terminal default color",
                                    ))
                                    .disabled(
                                        locked || pending || target_color_is_unset(&draft, target),
                                    )
                                    .on_click(cx.listener(
                                        move |panel, _: &ClickEvent, window, cx| {
                                            panel.update_keyword_editor_color(
                                                target, None, window, cx,
                                            );
                                        },
                                    )),
                            )
                    });
                div()
                    .flex_1()
                    .flex_basis(px(180.))
                    .min_w(px(0.))
                    .relative()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.control_radius)
                    .p_3()
                    .pt(px(16.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .absolute()
                            .left(px(8.))
                            .top(px(-8.))
                            .px_1()
                            .bg(theme.panel)
                            .text_size(theme.text_xs)
                            .child(title.clone()),
                    )
                    .children(rows)
            });
            editor = editor.child(
                div()
                    .id("keyword-custom-colors")
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .children(groups),
            );
        }
        editor = editor.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("终端效果预览", "Terminal preview")),
                )
                .child(
                    div()
                        .id("keyword-terminal-preview")
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .font_family(preview_font)
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .bg(gpui_kit::rgb(terminal_colors.background))
                        .text_color(gpui_kit::rgb(terminal_colors.foreground))
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_color(gpui_kit::rgb(terminal_colors.green))
                                .child("$ tail -f app.log"),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .text_color(gpui_kit::rgb(terminal_colors.cyan))
                                        .child("[12:34:56]"),
                                )
                                .child(" service returned ")
                                .child(
                                    div()
                                        .px_1()
                                        .bg(background)
                                        .text_color(foreground)
                                        .rounded_sm()
                                        .child(if draft.pattern.is_empty() {
                                            "Highlight".to_owned()
                                        } else {
                                            draft.pattern.clone()
                                        }),
                                ),
                        ),
                ),
        );

        if let Some(error) = error {
            editor = editor.child(
                div()
                    .id("keyword-editor-error")
                    .w_full()
                    .px_3()
                    .py_2()
                    .bg(theme.error.opacity(0.12))
                    .text_color(theme.error)
                    .rounded_md()
                    .text_size(theme.text_xs)
                    .child(error),
            );
        }

        let delete_button = if is_new {
            div().flex_1().into_any_element()
        } else {
            let delete_disabled = locked || pending;
            Button::new("keyword-delete")
                .danger()
                .icon(gpui_kit::assets::IconName::Trash)
                .label(crate::locale::text("删除规则", "Delete rule"))
                .disabled(delete_disabled)
                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                    panel.delete_selected_keyword_rule(window, cx);
                }))
                .into_any_element()
        };
        div()
            .id("keyword-rule-editor")
            .w_full()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .child(editor)
            .child(
                div()
                    .id("keyword-rule-editor-footer")
                    .debug_selector(|| "keyword-rule-editor-footer".into())
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(delete_button)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("keyword-cancel")
                                    .custom(super::render_settings::settings_ghost_variant(
                                        theme, cx,
                                    ))
                                    .icon(gpui_kit::assets::IconName::RotateCcw)
                                    .label(crate::locale::text("取消", "Cancel"))
                                    .disabled(locked || pending)
                                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                        panel.cancel_keyword_rule(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("keyword-save")
                                    .primary()
                                    .icon(gpui_kit::assets::IconName::Check)
                                    .label(if pending {
                                        crate::locale::text("保存中…", "Saving…")
                                    } else {
                                        crate::locale::text("保存规则", "Save rule")
                                    })
                                    .disabled(locked || pending)
                                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                        panel.save_keyword_rule(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

/// 匹配模式使用 main 的中性分段选中态，共享同一草稿入口而不引入独立保存动作。
///
/// @author kongweiguang
fn mode_button(
    mode: TerminalKeywordHighlightMatchMode,
    id: &'static str,
    label: gpui_kit::SharedString,
    selected: TerminalKeywordHighlightMatchMode,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> Button {
    Button::new(id)
        .compact()
        .custom(super::render_settings::settings_ghost_variant(theme, cx))
        .h(px(36.))
        .label(label)
        .flex_1()
        .disabled(disabled)
        .text_color(theme.settings_choice_foreground(mode == selected))
        .when(mode == selected, |button| {
            button.bg(theme.surface_selected())
        })
        .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
            panel.update_keyword_rule_draft(|rule| rule.match_mode = mode, cx);
        }))
}

/// 无备注、文本、整词和正则在列表及搜索中使用同一人类可读标签。
///
/// @author kongweiguang
fn match_mode_label(mode: TerminalKeywordHighlightMatchMode) -> &'static str {
    match mode {
        TerminalKeywordHighlightMatchMode::Literal => "文本",
        TerminalKeywordHighlightMatchMode::WholeWord => "整词",
        TerminalKeywordHighlightMatchMode::Regex => "正则",
    }
}

/// 将浅深主题预览并排绘制在稳定方形区域内，不因颜色值变化挤动规则行。
///
/// @author kongweiguang
fn color_pair_swatch(foreground: gpui_kit::Hsla, background: gpui_kit::Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(20.))
        .flex()
        .overflow_hidden()
        .rounded_sm()
        .border_1()
        .border_color(background)
        .child(
            div()
                .size(px(10.))
                .bg(background)
                .text_color(foreground)
                .child("A"),
        )
        .child(
            div()
                .size(px(10.))
                .bg(foreground)
                .text_color(background)
                .child("a"),
        )
}

/// 取当前规则的浅色预设或自定义色对，作为终端效果预览的稳定颜色来源。
///
/// @author kongweiguang
fn preview_colors(rule: &TerminalKeywordHighlightRule) -> (gpui_kit::Hsla, gpui_kit::Hsla) {
    let (foreground, background, _, _) = match rule.style {
        TerminalKeywordHighlightStyle::Custom => {
            let pair = rule.custom_colors.as_ref().map(|colors| &colors.light);
            (
                pair.and_then(|pair| pair.foreground.as_deref())
                    .unwrap_or("#854D0E"),
                pair.and_then(|pair| pair.background.as_deref())
                    .unwrap_or("#FEF9C3"),
                "#FDE047",
                "#422006",
            )
        }
        style => style_palette(style),
    };
    (
        parse_hex_color(foreground).unwrap_or_else(|| gpui_kit::rgb(0x854D0E).into()),
        parse_hex_color(background).unwrap_or_else(|| gpui_kit::rgb(0xFEF9C3).into()),
    )
}

/// 规则列表的颜色标记同时显示自适应浅色/深色色板，不把颜色误当作状态编码。
///
/// @author kongweiguang
fn rule_swatch_colors(
    rule: &TerminalKeywordHighlightRule,
) -> (
    (gpui_kit::Hsla, gpui_kit::Hsla),
    (gpui_kit::Hsla, gpui_kit::Hsla),
) {
    let (light_fg, light_bg, dark_fg, dark_bg) = match rule.style {
        TerminalKeywordHighlightStyle::Custom => {
            let colors = rule.custom_colors.as_ref();
            (
                colors
                    .and_then(|colors| colors.light.foreground.as_deref())
                    .unwrap_or("#854D0E"),
                colors
                    .and_then(|colors| colors.light.background.as_deref())
                    .unwrap_or("#FEF9C3"),
                colors
                    .and_then(|colors| colors.dark.foreground.as_deref())
                    .unwrap_or("#FDE047"),
                colors
                    .and_then(|colors| colors.dark.background.as_deref())
                    .unwrap_or("#422006"),
            )
        }
        style => style_palette(style),
    };
    (
        (
            parse_hex_color(light_fg).unwrap(),
            parse_hex_color(light_bg).unwrap(),
        ),
        (
            parse_hex_color(dark_fg).unwrap(),
            parse_hex_color(dark_bg).unwrap(),
        ),
    )
}

/// 颜色重置按钮只在已设置的单个字段上可用，保留其他主题和颜色值。
///
/// @author kongweiguang
fn target_color_is_unset(
    rule: &TerminalKeywordHighlightRule,
    target: super::panel::terminal::KeywordColorTarget,
) -> bool {
    target.setting_value(rule.custom_colors.as_ref()).is_none()
}

/// 给色板按钮的提示使用 main 同一中文名，英文 locale 下也保持可读。
///
/// @author kongweiguang
fn style_label(style: TerminalKeywordHighlightStyle) -> &'static str {
    match style {
        TerminalKeywordHighlightStyle::Red => "红色",
        TerminalKeywordHighlightStyle::Orange => "橙色",
        TerminalKeywordHighlightStyle::Yellow => "黄色",
        TerminalKeywordHighlightStyle::Green => "绿色",
        TerminalKeywordHighlightStyle::Cyan => "青色",
        TerminalKeywordHighlightStyle::Blue => "蓝色",
        TerminalKeywordHighlightStyle::Purple => "紫色",
        TerminalKeywordHighlightStyle::Pink => "粉色",
        TerminalKeywordHighlightStyle::Custom => "自定义色",
    }
}

/// 返回 main 的双主题八色值，正则/文本字段不参与颜色解析。
///
/// @author kongweiguang
fn style_palette(
    style: TerminalKeywordHighlightStyle,
) -> (&'static str, &'static str, &'static str, &'static str) {
    use TerminalKeywordHighlightStyle as Style;
    match style {
        Style::Red => ("#991B1B", "#FEE2E2", "#FCA5A5", "#450A0A"),
        Style::Orange => ("#9A3412", "#FFEDD5", "#FDBA74", "#431407"),
        Style::Yellow | Style::Custom => ("#854D0E", "#FEF9C3", "#FDE047", "#422006"),
        Style::Green => ("#166534", "#DCFCE7", "#86EFAC", "#052E16"),
        Style::Cyan => ("#155E75", "#CFFAFE", "#67E8F9", "#083344"),
        Style::Blue => ("#1E40AF", "#DBEAFE", "#93C5FD", "#172554"),
        Style::Purple => ("#6B21A8", "#F3E8FF", "#D8B4FE", "#3B0764"),
        Style::Pink => ("#9D174D", "#FCE7F3", "#F9A8D4", "#500724"),
    }
}
