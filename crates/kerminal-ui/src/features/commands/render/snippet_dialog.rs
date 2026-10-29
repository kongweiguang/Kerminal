//! 片段新增与编辑弹层；草稿仍由 CommandsView 持有，关闭只收起视图。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Window,
    assets::IconName,
    component::{
        ActiveTheme, Disableable,
        button::{Button, ButtonVariants},
        checkbox::Checkbox,
        input::{Input, InputContentType, Textarea},
        select::Select,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::commands::{SnippetCatalogOrigin, SnippetDefaultAction, SnippetPreferenceKey};

use crate::theme::ThemeTokens;

use super::super::super::snippet_browser::snippet_preview_copy_is_allowed;
use super::super::CommandsView;

impl CommandsView {
    /// 调用方统一传入 Window；布局只依赖 Context，窗口操作由各事件回调持有自己的 Window。
    /// 正文按固有内容高度参与布局；达到视口上限时正文收缩滚动，决策回执和固定操作仍可见。
    /// 元数据与命令字段按主分支顺序常驻显示；敏感命令仍可预览和运行，但剪贴板门禁与点击处理共用同一策略。
    /// 紧凑编辑器仅保存定义，宽工作台保留非敏感草稿的复制与执行入口。
    /// 紧凑底栏遵循 main 的 36px 控件与整钮禁用透明度，避免 Kit 默认把主操作替换成灰色文字。
    /// 紧凑命令框保持 main 的 128px 最小编辑高度；固定模态视口通过正文滚动容纳它。
    ///
    /// @author kongweiguang
    pub(crate) fn render_snippet_dialog_content(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let compact_layout = self.compact_layout;
        let primary_background = cx.theme().primary;
        let primary_foreground = cx.theme().primary_foreground;
        let selected_title = if compact_layout {
            if self.snippet_is_new && self.snippet_new_from_command {
                crate::locale::text("保存当前终端命令", "Save current terminal command")
            } else if self.snippet_is_new {
                crate::locale::text("新建命令片段", "New command snippet")
            } else {
                crate::locale::text("编辑命令片段", "Edit command snippet")
            }
        } else if self.snippet_is_new {
            crate::locale::text("新建片段", "New snippet")
        } else {
            crate::locale::text("编辑片段", "Edit snippet")
        };
        let writable = self
            .selected_snippet
            .as_ref()
            .is_none_or(|snippet| snippet.revision.is_some());
        let can_delete = self
            .selected_snippet
            .as_ref()
            .is_some_and(|selected| selected.revision.is_some());
        let send_in_flight = self.snippet_terminal_fill_in_flight();
        let variables = if compact_layout {
            Vec::new()
        } else {
            self.render_snippet_runtime_variables(theme, cx)
        };
        let show_variable_definitions =
            !self.snippet_variables.is_empty() || (writable && !compact_layout);
        let scope_control =
            self.render_snippet_scope_control(theme, writable && !send_in_flight, cx);
        let metadata = self.render_snippet_editor_metadata(theme, writable && !send_in_flight, cx);
        let text_details =
            self.render_snippet_editor_text_details(theme, writable && !send_in_flight);
        let run_primary = self.snippet_default_action == SnippetDefaultAction::Run;
        let fill_primary = self.snippet_default_action == SnippetDefaultAction::Insert;
        let send_disabled = self.snippet_terminal_fill_blocked()
            || self.pending_snippet_run.is_some()
            || self.snippet_saving
            || self.confirm_snippet_delete
            || self.pending_snippet_transition.is_some()
            || (!self.snippet_is_new && self.selected_snippet.is_none())
            || self.ui_session_binding.is_none()
            || self.snippet_execution.is_none();
        let title_empty = self.snippet_title.read(cx).value().trim().is_empty();
        let command_empty = self
            .snippet_command
            .read(cx)
            .text()
            .to_string()
            .trim()
            .is_empty();
        let save_disabled = self.snippet_saving
            || self.pending_snippet_transition.is_some()
            || send_in_flight
            || title_empty
            || command_empty
            || (!self.snippet_is_new && self.selected_snippet.is_none());
        let copy_preview_sensitive = self.selected_snippet_sensitive
            || self.snippet_variables.iter().any(|field| {
                field.variable.sensitive
                    || field.variable.kind == kerminal_domain::commands::SnippetVariableKind::Secret
            });
        let copy_preview_visible = !compact_layout
            && !self.snippet_variables.is_empty()
            && self.selected_snippet.is_some();
        let copy_preview_key = copy_preview_visible.then(|| {
            self.selected_snippet
                .as_ref()
                .map(|selected| SnippetPreferenceKey {
                    origin: if selected.revision.is_some() {
                        SnippetCatalogOrigin::User
                    } else {
                        SnippetCatalogOrigin::Builtin
                    },
                    snippet_id: selected.definition.id.clone(),
                })
        });
        let copy_preview_key = copy_preview_key.flatten();
        let copy_preview_disabled =
            self.snippet_saving || !snippet_preview_copy_is_allowed(copy_preview_sensitive);
        let pending_transition = self.pending_snippet_transition.is_some();
        let locked = self.snippet_saving || self.confirm_snippet_delete || pending_transition;
        let terminal_feedback =
            (!compact_layout && self.snippet_terminal_fill_blocked()).then(|| {
                self.render_snippet_fill_feedback(theme, cx)
                    .into_any_element()
            });
        let fixed_feedback = if pending_transition {
            Some(self.render_snippet_transition(theme, cx).into_any_element())
        } else {
            let conflict = self.configuration_conflict.clone();
            let status = self.status.clone();
            (conflict.is_some() || status.is_some()).then(|| {
                div()
                    .id("snippet-editor-status-fixed")
                    .w_full()
                    .flex_shrink_0()
                    .max_h(px(96.))
                    .overflow_y_scroll()
                    .px_4()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when_some(conflict, |feedback, conflict| {
                        feedback.child(
                            div()
                                .w_full()
                                .p_2()
                                .rounded_sm()
                                .bg(theme.warning.opacity(0.12))
                                .text_color(theme.warning)
                                .child(conflict),
                        )
                    })
                    .when_some(status, |feedback, status| {
                        feedback.child(
                            div()
                                .w_full()
                                .p_2()
                                .rounded_sm()
                                .bg(theme.materials.panel)
                                .text_size(theme.text_xs)
                                .text_color(theme.foreground)
                                .child(status),
                        )
                    })
                    .into_any_element()
            })
        };

        div()
            .id("snippet-editor-dialog")
            .w_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_color(theme.foreground)
            .child(
                div()
                    .id("snippet-editor-header")
                    .w_full()
                    .flex_shrink_0()
                    .px_5()
                    .py_4()
                    .flex()
                    .items_start()
                    .gap_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(17.))
                            .line_height(px(24.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(selected_title),
                    )
                    .child(
                        Button::new("snippet-editor-close")
                            .compact()
                            .ghost()
                            .w(px(36.))
                            .h(px(36.))
                            .rounded(theme.control_radius)
                            .icon(IconName::X)
                            .accessibility_label(crate::locale::text(
                                "关闭片段编辑器",
                                "Close snippet editor",
                            ))
                            .tooltip(crate::locale::text("关闭", "Close"))
                            .disabled(locked)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.close_snippet_editor(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("snippet-editor-body")
                    .flex_auto()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .when_some(terminal_feedback, |body, feedback| body.child(feedback))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_3()
                            .child(
                                div().flex_1().min_w(px(0.)).child(field_label(
                                    crate::locale::text("标题", "Title"),
                                    Input::new(&self.snippet_title)
                                        .disabled(
                                            !writable || self.snippet_saving || send_in_flight,
                                        )
                                        .w_full()
                                        .when(compact_layout, |input| {
                                            compact_snippet_field(input, theme)
                                        }),
                                    theme,
                                    compact_layout,
                                )),
                            )
                            .child(
                                div()
                                    .w(px(if compact_layout { 144. } else { 180. }))
                                    .flex_shrink_0()
                                    .child(field_label(
                                        crate::locale::text("作用域", "Scope"),
                                        scope_control,
                                        theme,
                                        compact_layout,
                                    )),
                            ),
                    )
                    .child(metadata)
                    .child(field_label(
                        crate::locale::text("命令模板", "Command template"),
                        Textarea::new(&self.snippet_command)
                            .disabled(!writable || self.snippet_saving || send_in_flight)
                            .w_full()
                            .when(compact_layout, |field| {
                                compact_snippet_control(field.h(px(128.)), theme)
                            }),
                        theme,
                        compact_layout,
                    ))
                    .when(!variables.is_empty(), |body| {
                        body.child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(section_title(
                                    crate::locale::text("运行时变量", "Runtime values"),
                                    theme,
                                ))
                                .children(variables),
                        )
                    })
                    .when(show_variable_definitions, |body| {
                        body.child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().min_w(px(0.)).child(section_title(
                                    if compact_layout {
                                        crate::locale::text("参数", "Parameters")
                                    } else if writable {
                                        crate::locale::text("模板变量定义", "Variable definitions")
                                    } else {
                                        crate::locale::text("变量定义", "Variable definitions")
                                    },
                                    theme,
                                )))
                                .when(writable && !compact_layout, |section| {
                                    section.child(
                                        Button::new("snippet-variable-add")
                                            .compact()
                                            .label(crate::locale::text("添加变量", "Add variable"))
                                            .disabled(self.snippet_saving || send_in_flight)
                                            .on_click(cx.listener(
                                                |view, _: &ClickEvent, window, cx| {
                                                    view.add_snippet_variable(window, cx);
                                                },
                                            )),
                                    )
                                }),
                        )
                    })
                    .when(show_variable_definitions, |body| {
                        body.children(self.render_snippet_variable_definitions(
                            theme,
                            writable && !send_in_flight,
                            compact_layout,
                            cx,
                        ))
                    })
                    .child(text_details)
                    .when(!compact_layout && self.confirm_snippet_delete, |body| {
                        body.child(self.render_snippet_delete_confirmation(theme, can_delete, cx))
                    })
                    .into_any_element(),
            )
            .when_some(fixed_feedback, |dialog, feedback| dialog.child(feedback))
            .child(
                div()
                    .id("snippet-editor-footer")
                    .w_full()
                    .flex_shrink_0()
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.materials.chrome)
                    .rounded_b(theme.radius_dialog)
                    .when(compact_layout, |footer| {
                        footer
                            .child(
                                Button::new("snippet-cancel")
                                    .ghost()
                                    .h(px(36.))
                                    .px(px(14.))
                                    .text_size(px(16.))
                                    .line_height(px(24.))
                                    .font_weight(gpui_kit::FontWeight::NORMAL)
                                    .text_color(theme.muted)
                                    .label(crate::locale::text("取消", "Cancel"))
                                    .disabled(self.snippet_saving)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.close_snippet_editor(cx);
                                    })),
                            )
                            .child(
                                Button::new("snippet-save")
                                    .primary()
                                    .h(px(36.))
                                    .px(px(14.))
                                    .text_size(px(16.))
                                    .line_height(px(24.))
                                    .font_weight(gpui_kit::FontWeight::NORMAL)
                                    .when(save_disabled, |button| {
                                        button
                                            .bg(primary_background)
                                            .border_color(primary_background)
                                            .text_color(primary_foreground)
                                            .opacity(0.5)
                                    })
                                    .icon(IconName::Save)
                                    .label(if self.snippet_saving {
                                        crate::locale::text("保存中...", "Saving...")
                                    } else {
                                        crate::locale::text("保存", "Save")
                                    })
                                    .disabled(save_disabled)
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        if view
                                            .selected_snippet
                                            .as_ref()
                                            .is_some_and(|selected| selected.revision.is_none())
                                        {
                                            view.save_builtin_snippet_as_user(window, cx);
                                        } else {
                                            view.save_snippet(window, cx);
                                        }
                                    })),
                            )
                    })
                    .when(!compact_layout, |footer| {
                        footer.child(
                            Button::new("snippet-fill")
                                .compact()
                                .when(fill_primary, Button::primary)
                                .when(!fill_primary, Button::secondary)
                                .label(crate::locale::text("填入草稿", "Fill draft"))
                                .disabled(
                                    self.snippet_saving
                                        || self.confirm_snippet_delete
                                        || pending_transition
                                        || send_in_flight,
                                )
                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                    view.fill_snippet(window, cx);
                                })),
                        )
                    })
                    .when_some(copy_preview_key, |footer, key| {
                        footer.child(
                            Button::new("snippet-copy-preview")
                                .compact()
                                .ghost()
                                .label(crate::locale::text("复制展开命令", "Copy rendered command"))
                                .accessibility_label(crate::locale::text(
                                    "复制展开命令",
                                    "Copy rendered command",
                                ))
                                .tooltip(if copy_preview_sensitive {
                                    crate::locale::text(
                                        "含敏感值的命令不能复制",
                                        "Commands with sensitive values cannot be copied",
                                    )
                                } else {
                                    crate::locale::text("复制展开命令", "Copy rendered command")
                                })
                                .disabled(copy_preview_disabled)
                                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                    view.copy_snippet_preview(key.clone(), cx);
                                })),
                        )
                    })
                    .when(!compact_layout, |footer| {
                        footer.child(
                            Button::new("snippet-send")
                                .compact()
                                .when(run_primary, Button::primary)
                                .when(!run_primary, Button::secondary)
                                .label(crate::locale::text("发送", "Send"))
                                .disabled(send_disabled)
                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                    view.send_snippet_terminal(window, cx);
                                })),
                        )
                    })
                    .when(writable && !compact_layout, |footer| {
                        footer
                            .child(
                                Button::new("snippet-delete")
                                    .compact()
                                    .danger()
                                    .label(crate::locale::text("删除", "Delete"))
                                    .disabled(
                                        !can_delete
                                            || self.snippet_saving
                                            || self.confirm_snippet_delete
                                            || send_in_flight,
                                    )
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.delete_snippet(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("snippet-save")
                                    .compact()
                                    .secondary()
                                    .label(crate::locale::text("保存", "Save"))
                                    .disabled(
                                        self.snippet_saving
                                            || self.pending_snippet_transition.is_some()
                                            || send_in_flight,
                                    )
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.save_snippet(window, cx);
                                    })),
                            )
                    })
                    .when(!writable && !compact_layout, |footer| {
                        footer.child(
                            Button::new("snippet-save-as-user")
                                .compact()
                                .primary()
                                .label(if self.builtin_copy_requires_reconciliation() {
                                    crate::locale::text("查询保存结果", "Check save result")
                                } else {
                                    crate::locale::text("保存到我的", "Save to My")
                                })
                                .disabled(locked || send_in_flight)
                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                    view.save_builtin_snippet_as_user(window, cx);
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    /// 编辑器与列表预览共用同一组运行输入实体，避免重复控件状态或丢失焦点/IME。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_runtime_variables(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Vec<gpui_kit::AnyElement> {
        let send_in_flight = self.snippet_terminal_fill_in_flight();
        self.snippet_variables
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let sensitive = field.variable.sensitive;
                let required = field.variable.required;
                let edited_label = field.label.read(cx).value().trim().to_owned();
                let runtime_label = if edited_label.is_empty() {
                    let edited_name = field.name.read(cx).value().trim().to_owned();
                    if edited_name.is_empty() {
                        field.variable.name.clone()
                    } else {
                        edited_name
                    }
                } else {
                    edited_label
                };
                div()
                    .id(format!("snippet-runtime-variable-{index}"))
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(theme.foreground)
                                    .child(runtime_label),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(if required {
                                        crate::locale::text("必填", "Required")
                                    } else {
                                        crate::locale::text("可选", "Optional")
                                    }),
                            ),
                    )
                    .child(
                        Input::new(&field.input)
                            .disabled(self.snippet_saving || send_in_flight)
                            .when(sensitive, |input| {
                                input.content_type(InputContentType::Password).mask_toggle()
                            })
                            .w_full(),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// compact 参数名称由模板推导并复用 main 的元数据字段；宽工作台保留名称与变量管理能力。
    ///
    /// @author kongweiguang
    pub(in crate::features::commands::render) fn render_snippet_variable_definitions(
        &self,
        theme: ThemeTokens,
        writable: bool,
        compact_layout: bool,
        cx: &mut Context<Self>,
    ) -> Vec<impl IntoElement> {
        self.snippet_variables
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let sensitive = field.variable.sensitive;
                let required = field.variable.required;
                let identity_fields = if compact_layout {
                    vec![
                        field_label(
                            crate::locale::text(
                                format!("{} 显示名", field.variable.name),
                                format!("{} display name", field.variable.name),
                            ),
                            compact_snippet_field(
                                Input::new(&field.label).disabled(!writable).w_full(),
                                theme,
                            ),
                            theme,
                            compact_layout,
                        )
                        .into_any_element(),
                    ]
                } else {
                    vec![
                        field_label(
                            crate::locale::text("变量名", "Variable name"),
                            Input::new(&field.name).disabled(!writable).w_full(),
                            theme,
                            compact_layout,
                        )
                        .into_any_element(),
                        field_label(
                            crate::locale::text("显示名称", "Display label"),
                            Input::new(&field.label).disabled(!writable).w_full(),
                            theme,
                            compact_layout,
                        )
                        .into_any_element(),
                    ]
                };
                let kind_selector = Select::new(&field.kind_select)
                    .w_full()
                    .disabled(!writable)
                    .accessibility_label(crate::locale::text("变量类型", "Variable kind"));
                let kind_selector = if compact_layout {
                    compact_snippet_field(kind_selector, theme)
                } else {
                    kind_selector.h(px(34.))
                };
                let strategy_selector = Select::new(&field.render_strategy_select)
                    .w_full()
                    .disabled(!writable)
                    .accessibility_label(crate::locale::text(
                        "变量插值策略",
                        "Variable rendering strategy",
                    ));
                let strategy_selector = if compact_layout {
                    compact_snippet_field(strategy_selector, theme)
                } else {
                    strategy_selector.h(px(34.))
                };
                let suggestions = (!sensitive).then(|| {
                    field_label(
                        crate::locale::text(
                            "建议选项（逗号分隔；枚举类型用作可选值）",
                            "Suggestions (comma separated; Enum uses them as choices)",
                        ),
                        Input::new(&field.suggestions)
                            .disabled(!writable)
                            .w_full()
                            .when(compact_layout, |input| compact_snippet_field(input, theme)),
                        theme,
                        compact_layout,
                    )
                    .into_any_element()
                });
                let mut card = div()
                    .id(format!("snippet-variable-definition-{index}"))
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .children(identity_fields)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_3()
                            .child(div().flex_1().min_w(px(220.)).child(field_label(
                                crate::locale::text("类型", "Kind"),
                                kind_selector,
                                theme,
                                compact_layout,
                            )))
                            .child(div().flex_1().min_w(px(220.)).child(field_label(
                                crate::locale::text("插值方式", "Rendering"),
                                strategy_selector,
                                theme,
                                compact_layout,
                            ))),
                    )
                    .child(field_label(
                        crate::locale::text("变量说明", "Variable description"),
                        Input::new(&field.description)
                            .disabled(!writable)
                            .w_full()
                            .when(compact_layout, |input| compact_snippet_field(input, theme)),
                        theme,
                        compact_layout,
                    ))
                    .child(field_label(
                        crate::locale::text(
                            "验证规则（可选安全正则）",
                            "Validation pattern (optional safe regex)",
                        ),
                        Input::new(&field.validation)
                            .disabled(!writable)
                            .w_full()
                            .when(compact_layout, |input| compact_snippet_field(input, theme)),
                        theme,
                        compact_layout,
                    ));

                if compact_layout {
                    let owner = cx.entity();
                    let variable_id = field.id;
                    card = card.child(
                        div()
                            .w_full()
                            .flex()
                            .items_end()
                            .gap_3()
                            .when_some(suggestions, |row, suggestions| {
                                row.child(div().flex_1().min_w(px(0.)).child(suggestions))
                            })
                            .child(
                                Checkbox::new(format!("snippet-variable-required-{index}"))
                                    .checked(required)
                                    .disabled(!writable)
                                    .label(crate::locale::text("必填", "Required"))
                                    .on_change(move |checked, _, app| {
                                        owner.update(app, |view, cx| {
                                            view.set_snippet_variable_required(
                                                variable_id,
                                                *checked,
                                                cx,
                                            );
                                        });
                                    }),
                            ),
                    );
                } else {
                    card = card
                        .when_some(suggestions, |row, suggestions| row.child(suggestions))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    Button::new(format!("snippet-variable-required-{index}"))
                                        .compact()
                                        .label(crate::locale::text(
                                            if required { "必填" } else { "可选" },
                                            if required { "Required" } else { "Optional" },
                                        ))
                                        .disabled(!writable)
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, _, cx| {
                                                view.toggle_snippet_variable_required(index, cx);
                                            },
                                        )),
                                )
                                .child(
                                    Button::new(format!("snippet-variable-sensitive-{index}"))
                                        .compact()
                                        .label(crate::locale::text(
                                            if sensitive {
                                                "敏感值"
                                            } else {
                                                "设为敏感"
                                            },
                                            if sensitive { "Secret" } else { "Mark secret" },
                                        ))
                                        .disabled(!writable)
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, window, cx| {
                                                view.toggle_snippet_variable_sensitive(
                                                    index, window, cx,
                                                );
                                            },
                                        )),
                                )
                                .child(
                                    Button::new(format!("snippet-variable-remove-{index}"))
                                        .compact()
                                        .label(crate::locale::text("移除变量", "Remove variable"))
                                        .disabled(!writable)
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, _, cx| {
                                                view.remove_snippet_variable(index, cx);
                                            },
                                        )),
                                ),
                        );
                }
                card
            })
            .collect()
    }

    /// 固定展示版本决策并说明运行变量生命周期；保存、放弃或取消仍严格走现有状态转换。
    ///
    /// @author kongweiguang
    fn render_snippet_transition(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("snippet-transition-decision")
            .w_full()
            .flex_shrink_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .bg(theme.materials.chrome)
            .child(crate::locale::text(
                "切换片段会清空本次运行变量；保存只保存片段定义。",
                "Switching snippets clears this run's variable values; saving only saves the snippet definition.",
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("snippet-transition-save")
                            .compact()
                            .primary()
                            .label(crate::locale::text(
                                "保存定义并继续",
                                "Save definition and continue",
                            ))
                            .disabled(self.snippet_saving)
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                view.save_snippet(window, cx);
                            })),
                    )
                    .child(
                        Button::new("snippet-transition-discard")
                            .compact()
                            .label(crate::locale::text(
                                "放弃本次输入并继续",
                                "Discard these inputs and continue",
                            ))
                            .disabled(self.snippet_saving)
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                view.discard_snippet_transition(window, cx);
                            })),
                    )
                    .child(
                        Button::new("snippet-transition-cancel")
                            .compact()
                            .label(crate::locale::text("取消切换", "Cancel switch"))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.cancel_snippet_transition(cx);
                            })),
                    ),
            )
    }

    /// 删除沿用已保存 revision 的二次确认；只读内置项不会出现可执行按钮。
    ///
    /// @author kongweiguang
    fn render_snippet_delete_confirmation(
        &mut self,
        theme: ThemeTokens,
        can_delete: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("snippet-delete-confirmation")
            .w_full()
            .p_3()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .rounded_md()
            .bg(theme.materials.chrome)
            .child(crate::locale::text(
                "确认删除此用户片段？此操作不会删除内置片段。",
                "Delete this user snippet? Built-in snippets cannot be deleted.",
            ))
            .child(
                Button::new("snippet-delete-confirm")
                    .compact()
                    .danger()
                    .label(crate::locale::text("确认删除", "Delete"))
                    .disabled(!can_delete || self.snippet_saving)
                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                        view.confirm_delete_snippet(window, cx);
                    })),
            )
            .child(
                Button::new("snippet-delete-cancel")
                    .compact()
                    .label(crate::locale::text("保留", "Keep"))
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.cancel_snippet_delete(cx);
                    })),
            )
    }
}

/// 显式标签保持字段含义可见，输入值不会只依赖 placeholder 才能辨认。
/// 紧凑表单复刻 main 的 24px 父行盒；中文 Medium 在浏览器实际选中常规字形，原生也显式使用常规字重。
///
/// @author kongweiguang
fn field_label(
    label: gpui_kit::SharedString,
    input: impl IntoElement,
    theme: ThemeTokens,
    compact_layout: bool,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .when(!compact_layout, |label| label.gap_1())
        .child(
            div()
                .text_size(theme.text_xs)
                .when(compact_layout, |label| {
                    label
                        .line_height(px(24.))
                        .font_weight(gpui_kit::FontWeight::NORMAL)
                })
                .text_color(theme.field_label)
                .child(label),
        )
        .child(input)
}

/// 紧凑右栏字段沿用 main 的 16/24 文本节奏与主题化字段表面；中央表单继续使用 Kit 默认控件样式。
///
/// @author kongweiguang
pub(super) fn compact_snippet_control<T: gpui_kit::Styled>(control: T, theme: ThemeTokens) -> T {
    control
        .text_size(px(16.))
        .line_height(px(24.))
        .font_weight(gpui_kit::FontWeight::NORMAL)
        .bg(theme.field_surface())
        .border_color(theme.strong_border())
}

/// 单行 Input 的 inherent `h` 仅控制多行模式，因此右栏外框高度必须通过 Styled refinement 设置。
///
/// @author kongweiguang
pub(in crate::features::commands::render) fn compact_snippet_field<T: gpui_kit::Styled>(
    control: T,
    theme: ThemeTokens,
) -> T {
    gpui_kit::Styled::h(compact_snippet_control(control, theme), px(36.))
}

/// 弹层的小节标题沿用当前主题的常规字号，不扩展成高对比卡片。
///
/// @author kongweiguang
fn section_title(label: gpui_kit::SharedString, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_sm)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(label)
}
