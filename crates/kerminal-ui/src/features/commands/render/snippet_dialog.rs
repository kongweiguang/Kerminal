//! 片段新增与编辑弹层；草稿仍由 CommandsView 持有，关闭只收起视图。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, SharedString, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::{Input, InputContentType, Textarea},
        select::Select,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::commands::{
    SnippetApplicabilityProjection, SnippetRunHistory, SnippetRunResult, SnippetRunTargetSummary,
    SnippetRunUsage,
};
use kerminal_domain::commands::{
    SnippetApplicabilityReason, SnippetApplicabilityStatus, SnippetCatalogOrigin,
    SnippetDefaultAction, SnippetPreferenceKey, SnippetRisk,
};

use crate::theme::ThemeTokens;

use super::super::super::{
    snippet_applicability::SnippetApplicabilityPhase, snippet_run::SnippetRunPhase,
};
use super::super::CommandsView;

impl CommandsView {
    /// 调用方统一传入 Window；布局只依赖 Context，窗口操作由各事件回调持有自己的 Window。
    /// 正文按固有内容高度参与布局；达到视口上限时正文收缩滚动，决策回执和固定操作仍可见。
    /// Fill 与保存/删除/切换决策互斥，避免入口外观暗示可绕过编辑状态保护。
    ///
    /// @author kongweiguang
    pub(crate) fn render_snippet_dialog_content(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let selected_title = self
            .selected_snippet
            .as_ref()
            .map(|selected| selected.definition.title.clone())
            .unwrap_or_else(|| crate::locale::text("新建片段", "New snippet").to_string());
        let writable = self
            .selected_snippet
            .as_ref()
            .is_none_or(|snippet| snippet.revision.is_some());
        let can_delete = self
            .selected_snippet
            .as_ref()
            .is_some_and(|selected| selected.revision.is_some());
        let run_blocks_editing = self.snippet_run_state().blocks_editing();
        let run_panel_open = self.snippet_run_state().is_open();
        let run_phase = self.snippet_run_state().phase();
        let applicability = self.render_selected_snippet_applicability(theme, cx);
        let variables = self.render_snippet_runtime_variables(theme, cx);
        let advanced = self.render_snippet_advanced(theme, writable && !run_blocks_editing, cx);
        let run_panel = run_panel_open.then(|| self.render_snippet_run_panel(theme, cx));
        let run_can_start = self.snippet_run_state().can_start();
        let run_primary = self.snippet_default_action == SnippetDefaultAction::Run;
        let fill_primary = self.snippet_default_action == SnippetDefaultAction::Insert;
        let reopening_run_state = matches!(
            run_phase,
            SnippetRunPhase::AwaitingConfirmation | SnippetRunPhase::OutcomeUnknown
        );
        let run_disabled = if reopening_run_state {
            false
        } else {
            !run_can_start
                || self.snippet_is_new
                || self.snippet_saving
                || self.pending_snippet_transition.is_some()
                || self.selected_snippet.is_none()
                || self.ui_session_binding.is_none()
                || self.selected_snippet_is_incompatible()
        };
        let run_label = match run_phase {
            SnippetRunPhase::AwaitingConfirmation => {
                crate::locale::text("查看待确认", "Review pending run")
            }
            SnippetRunPhase::OutcomeUnknown => {
                crate::locale::text("查看上次运行", "Review previous run")
            }
            _ => crate::locale::text("运行", "Run"),
        };
        let copy_preview_sensitive = self
            .snippet_variables
            .iter()
            .any(|field| field.variable.sensitive);
        let copy_preview_visible = !self.snippet_variables.is_empty()
            && self.selected_snippet.is_some()
            && self.ui_session_binding.is_some()
            && !matches!(
                run_phase,
                SnippetRunPhase::Preparing
                    | SnippetRunPhase::AwaitingConfirmation
                    | SnippetRunPhase::Running
                    | SnippetRunPhase::CancellationRequested
            );
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
        let copy_preview_disabled = self.snippet_saving;
        let pending_transition = self.pending_snippet_transition.is_some();
        let locked = self.snippet_saving || self.confirm_snippet_delete || pending_transition;
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
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(theme.text_heading)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(selected_title),
                    )
                    .child(
                        Button::new("snippet-editor-close")
                            .compact()
                            .label(crate::locale::text("关闭", "Close"))
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
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(field_label(
                        crate::locale::text("标题", "Title"),
                        Input::new(&self.snippet_title)
                            .disabled(!writable || self.snippet_saving || run_blocks_editing)
                            .w_full(),
                        theme,
                    ))
                    .child(field_label(
                        crate::locale::text("命令模板", "Command template"),
                        Textarea::new(&self.snippet_command)
                            .disabled(!writable || self.snippet_saving || run_blocks_editing)
                            .w_full(),
                        theme,
                    ))
                    .when_some(applicability, |body, applicability| {
                        body.child(applicability)
                    })
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
                    .when(writable, |body| {
                        body.child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().min_w(px(0.)).child(section_title(
                                    crate::locale::text("模板变量定义", "Variable definitions"),
                                    theme,
                                )))
                                .child(
                                    Button::new("snippet-variable-add")
                                        .compact()
                                        .label(crate::locale::text("添加变量", "Add variable"))
                                        .disabled(self.snippet_saving || run_blocks_editing)
                                        .on_click(cx.listener(
                                            |view, _: &ClickEvent, window, cx| {
                                                view.add_snippet_variable(window, cx);
                                            },
                                        )),
                                ),
                        )
                    })
                    .when(writable, |body| {
                        body.children(self.render_snippet_variable_definitions(
                            theme,
                            writable && !run_blocks_editing,
                            cx,
                        ))
                    })
                    .child(advanced)
            .when_some(run_panel, |body, panel| body.child(panel))
                    .when(self.confirm_snippet_delete, |body| {
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
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.materials.chrome)
                    .child(
                        Button::new("snippet-fill")
                            .compact()
                            .when(fill_primary, Button::primary)
                            .when(!fill_primary, Button::secondary)
                            .label(if self.compact_layout {
                                crate::locale::text("填入终端", "Insert into terminal")
                            } else {
                                crate::locale::text("填入命令草稿", "Insert into draft")
                            })
                            .disabled(
                                self.selected_snippet.is_none()
                                    || self.session_id.is_none()
                                    || self.snippet_saving
                                    || self.confirm_snippet_delete
                                    || pending_transition
                                    || run_blocks_editing,
                            )
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                view.fill_snippet(window, cx);
                            })),
                    )
                    .when_some(copy_preview_key, |footer, key| {
                        footer.child(
                            Button::new("snippet-copy-preview")
                                .compact()
                                .ghost()
                                .label(if copy_preview_sensitive {
                                    crate::locale::text("复制含敏感值的命令", "Copy command with sensitive values")
                                } else {
                                    crate::locale::text("复制展开命令", "Copy rendered command")
                                })
                                .accessibility_label(if copy_preview_sensitive {
                                    crate::locale::text(
                                        "复制展开命令；敏感值会进入系统剪贴板，不会写入历史",
                                        "Copy rendered command; sensitive values go to the system clipboard and are not written to history",
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
                    .child(
                        Button::new("snippet-run")
                            .compact()
                            .when(run_primary, Button::primary)
                            .when(!run_primary, Button::secondary)
                            .label(run_label)
                            .disabled(run_disabled)
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                view.prepare_snippet_run(window, cx);
                            })),
                    )
                    .when(writable, |footer| {
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
                                            || run_blocks_editing,
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
                                            || run_blocks_editing,
                                    )
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.save_snippet(window, cx);
                                    })),
                            )
                    })
                    .when(!writable, |footer| {
                        footer.child(
                            Button::new("snippet-save-as-user")
                                .compact()
                                .primary()
                                .label(if self.builtin_copy_requires_reconciliation() {
                                    crate::locale::text("查询保存结果", "Check save result")
                                } else {
                                    crate::locale::text("保存到我的", "Save to My")
                                })
                                .disabled(locked || run_blocks_editing)
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
        let run_blocks_editing = self.snippet_run_state().blocks_editing();
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
                            .disabled(self.snippet_saving || run_blocks_editing)
                            .when(sensitive, |input| {
                                input.content_type(InputContentType::Password).mask_toggle()
                            })
                            .w_full(),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// 展示当前保存定义与确切 UI binding 的只读评估；pending 和失败不冒充兼容。
    ///
    /// @author kongweiguang
    pub(super) fn render_selected_snippet_applicability(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        let phase = self.selected_snippet_applicability()?.clone();
        let compact_hint = matches!(
            &phase,
            SnippetApplicabilityPhase::Ready(projection)
                if projection.assessment.status == SnippetApplicabilityStatus::Unrestricted
        );
        let (headline, detail, tone, declarations, retryable) = match phase {
            SnippetApplicabilityPhase::Loading => (
                crate::locale::text("正在核对片段适用性…", "Checking snippet applicability…"),
                Some(crate::locale::text(
                    "正在按当前条目和绑定的终端读取声明。",
                    "Reading declarations for this exact entry and bound terminal.",
                )),
                theme.muted,
                None,
                false,
            ),
            SnippetApplicabilityPhase::Failed(error) => (
                crate::locale::text("适用性暂时无法核对", "Applicability could not be checked"),
                Some(crate::locale::text(error.clone(), error)),
                theme.error,
                None,
                true,
            ),
            SnippetApplicabilityPhase::Ready(projection) => {
                let (headline, detail, tone) = applicability_copy(&projection, theme);
                let declarations = applicability_declarations(&projection);
                (headline, detail, tone, declarations, false)
            }
        };

        let panel = div()
            .id("snippet-applicability-status")
            .w_full()
            .min_w(px(0.))
            .when(compact_hint, |panel| panel.py_1())
            .when(!compact_hint, |panel| {
                panel
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(tone.opacity(0.5))
                    .bg(theme.materials.chrome)
            })
            .child(
                div()
                    .text_size(if compact_hint {
                        theme.text_xs
                    } else {
                        theme.text_sm
                    })
                    .text_color(if compact_hint { theme.muted } else { tone })
                    .child(headline),
            )
            .when_some(detail, |panel, detail| {
                panel.child(
                    div()
                        .w_full()
                        .text_size(theme.text_xs)
                        .text_color(if retryable { theme.error } else { theme.muted })
                        .child(detail),
                )
            })
            .when_some(declarations, |panel, declarations| {
                panel.child(
                    div()
                        .w_full()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(declarations),
                )
            })
            .when(retryable, |panel| {
                panel.child(
                    Button::new("snippet-applicability-retry")
                        .compact()
                        .secondary()
                        .label(crate::locale::text("重试核对", "Retry check"))
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            view.retry_selected_snippet_applicability(cx);
                        })),
                )
            });
        Some(panel.into_any_element())
    }

    /// 每个可编辑变量定义与运行期输入分区展示，避免把默认值误认为本次秘密值。
    ///
    /// @author kongweiguang
    pub(in crate::features::commands::render) fn render_snippet_variable_definitions(
        &self,
        theme: ThemeTokens,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> Vec<impl IntoElement> {
        self.snippet_variables
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let sensitive = field.variable.sensitive;
                let required = field.variable.required;
                let kind_selector = Select::new(&field.kind_select)
                    .w_full()
                    .h(px(34.))
                    .disabled(!writable)
                    .accessibility_label(crate::locale::text("变量类型", "Variable kind"));
                let strategy_selector = Select::new(&field.render_strategy_select)
                    .w_full()
                    .h(px(34.))
                    .disabled(!writable)
                    .accessibility_label(crate::locale::text(
                        "变量插值策略",
                        "Variable rendering strategy",
                    ));
                div()
                    .id(format!("snippet-variable-definition-{index}"))
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(field_label(
                        crate::locale::text("变量名", "Variable name"),
                        Input::new(&field.name).disabled(!writable).w_full(),
                        theme,
                    ))
                    .child(field_label(
                        crate::locale::text("显示名称", "Display label"),
                        Input::new(&field.label).disabled(!writable).w_full(),
                        theme,
                    ))
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
                            )))
                            .child(div().flex_1().min_w(px(220.)).child(field_label(
                                crate::locale::text("插值方式", "Rendering"),
                                strategy_selector,
                                theme,
                            ))),
                    )
                    .child(field_label(
                        crate::locale::text("变量说明", "Variable description"),
                        Input::new(&field.description).disabled(!writable).w_full(),
                        theme,
                    ))
                    .child(field_label(
                        crate::locale::text(
                            "验证规则（可选安全正则）",
                            "Validation pattern (optional safe regex)",
                        ),
                        Input::new(&field.validation).disabled(!writable).w_full(),
                        theme,
                    ))
                    .when(!sensitive, |row| {
                        row.child(field_label(
                            crate::locale::text(
                                "建议选项（逗号分隔；枚举类型用作可选值）",
                                "Suggestions (comma separated; Enum uses them as choices)",
                            ),
                            Input::new(&field.suggestions).disabled(!writable).w_full(),
                            theme,
                        ))
                    })
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
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.toggle_snippet_variable_required(index, cx);
                                    })),
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
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.remove_snippet_variable(index, cx);
                                    })),
                            ),
                    )
            })
            .collect()
    }

    /// 运行预览与结果留在同一编辑面板；待确认和未知结果不可收起，防止唯一确认/核对入口丢失。
    /// 拥有化结果不延长视图借用，目标永不从后来选中的 Tab 推断。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_run_panel(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let state = self.snippet_run_state();
        let phase = state.phase();
        let info = state.confirmation().cloned();
        let result = state.result().cloned();
        let status = state.status().map(str::to_owned);
        let error = state.error().map(str::to_owned);
        let busy = state.is_busy();
        let session_id = info
            .as_ref()
            .map(|info| info.session_id.clone())
            .or_else(|| state.unknown_session_id().cloned());
        let session_label = session_id.as_ref().map(short_session_id);
        let phase_body = match phase {
            SnippetRunPhase::AwaitingConfirmation => {
                let info = info.clone();
                div()
                    .id("snippet-run-confirmation")
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when_some(info, |confirmation, info| {
                        let target = run_target_label(info.target);
                        let captured_session = short_session_id(&info.session_id);
                        confirmation
                            .child(run_labeled_value(
                                crate::locale::text("目标", "Target"),
                                crate::locale::text(
                                    format!("{target} · 会话 {captured_session}"),
                                    format!("{target} · Session {captured_session}"),
                                ),
                                theme,
                            ))
                            .child(run_labeled_value(
                                crate::locale::text("片段", "Snippet"),
                                info.title.clone(),
                                theme,
                            ))
                            .child(run_labeled_value(
                                crate::locale::text("风险", "Risk"),
                                run_risk_label(info.risk),
                                theme,
                            ))
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(theme.text_xs)
                                            .text_color(theme.muted)
                                            .child(crate::locale::text(
                                                "命令预览",
                                                "Command preview",
                                            )),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .p_3()
                                            .rounded_md()
                                            .bg(theme.materials.panel)
                                            .text_color(theme.foreground)
                                            .child(info.command_preview.clone()),
                                    ),
                            )
                            .when(info.sensitive, |panel| {
                                panel.child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.warning)
                                        .child(crate::locale::text(
                                            "敏感变量仅按模板预览；展开值不会写入历史。",
                                            "Sensitive values are shown as template placeholders and are excluded from history.",
                                        )),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        Button::new("snippet-run-cancel-confirmation")
                                            .compact()
                                            .label(crate::locale::text(
                                                "取消确认",
                                                "Cancel run",
                                            ))
                                            .on_click(cx.listener(
                                                |view, _: &ClickEvent, _, cx| {
                                                    view.cancel_snippet_run(cx);
                                                },
                                            )),
                                    )
                                    .child(
                                        Button::new("snippet-run-confirm")
                                            .compact()
                                            .primary()
                                            .label(crate::locale::text(
                                                "确认并运行",
                                                "Confirm and run",
                                            ))
                                            .on_click(cx.listener(
                                                |view, _: &ClickEvent, window, cx| {
                                                    view.confirm_snippet_run(window, cx);
                                                },
                                            )),
                                    ),
                            )
                    })
                    .into_any_element()
            }
            SnippetRunPhase::OutcomeUnknown => div()
                .id("snippet-run-unknown")
                .w_full()
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .rounded_md()
                .bg(theme.warning.opacity(0.1))
                .text_color(theme.foreground)
                .child(crate::locale::text(
                    "终端回执未知。请先检查原目标的终端；本次不会自动重放。",
                    "The terminal receipt is unknown. Inspect the original terminal; this run will not be replayed.",
                ))
                .when_some(session_label.clone(), |panel, session| {
                    panel.child(crate::locale::text(
                        format!("原会话：{session}"),
                        format!("Original session: {session}"),
                    ))
                })
                .child(
                    Button::new("snippet-run-unknown-acknowledge")
                        .compact()
                        .label(crate::locale::text(
                            "我已检查原终端，结束本次记录",
                            "I checked the original terminal; close this record",
                        ))
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            view.acknowledge_unknown_after_terminal_review(cx);
                        })),
                )
                .into_any_element(),
            SnippetRunPhase::Preparing => div()
                .id("snippet-run-preparing")
                .w_full()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(crate::locale::text(
                    "正在准备安全预览…",
                    "Preparing the run preview…",
                ))
                .child(
                    Button::new("snippet-run-cancel-preparation")
                        .compact()
                        .label(crate::locale::text("取消", "Cancel"))
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            view.cancel_snippet_run(cx);
                        })),
                )
                .into_any_element(),
            SnippetRunPhase::Running | SnippetRunPhase::CancellationRequested => {
                div()
                    .id("snippet-run-running")
                    .w_full()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(crate::locale::text(
                        if phase == SnippetRunPhase::CancellationRequested {
                            "已请求取消，正在等待终端回执…"
                        } else {
                            "命令已提交，正在等待终端回执…"
                        },
                        if phase == SnippetRunPhase::CancellationRequested {
                            "Cancellation requested; waiting for the terminal receipt…"
                        } else {
                            "Command submitted; waiting for the terminal receipt…"
                        },
                    ))
                    .when(phase == SnippetRunPhase::Running, |panel| {
                        panel.child(
                            Button::new("snippet-run-cancel")
                                .compact()
                                .label(crate::locale::text("请求取消", "Request cancellation"))
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.cancel_snippet_run(cx);
                                })),
                        )
                    })
                    .into_any_element()
            }
            SnippetRunPhase::Finished => result
                .as_ref()
                .map(|result| render_run_result(result, theme))
                .unwrap_or_else(|| {
                    phase_status_panel(
                        "snippet-run-finished",
                        crate::locale::text("终端已确认完成。", "The terminal confirmed completion."),
                        theme,
                    )
                }),
            SnippetRunPhase::OutcomeReviewed => phase_status_panel(
                "snippet-run-reviewed",
                crate::locale::text(
                    "本次未知结果已由你检查并结束记录；再次运行需要新的明确操作。",
                    "You reviewed and closed the unknown result; another run needs a new explicit action.",
                ),
                theme,
            ),
            SnippetRunPhase::Cancelled => phase_status_panel(
                "snippet-run-cancelled",
                crate::locale::text(
                    "准备已取消；这不表示终端命令已停止。",
                    "Preparation was cancelled; this does not claim a submitted command has stopped.",
                ),
                theme,
            ),
            SnippetRunPhase::Failed => phase_status_panel(
                "snippet-run-failed",
                crate::locale::text("运行未能完成。", "The run could not be completed."),
                theme,
            ),
            SnippetRunPhase::Idle => div().id("snippet-run-idle").into_any_element(),
        };

        div()
            .id("snippet-run-panel")
            .w_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.materials.chrome)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(crate::locale::text("片段运行", "Snippet run")),
                    )
                    .when(
                        !busy
                            && !matches!(
                                phase,
                                SnippetRunPhase::AwaitingConfirmation
                                    | SnippetRunPhase::OutcomeUnknown
                            ),
                        |header| {
                            header.child(
                                Button::new("snippet-run-hide")
                                    .compact()
                                    .ghost()
                                    .label(crate::locale::text("收起", "Hide"))
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.close_snippet_run_modal(cx);
                                    })),
                            )
                        },
                    ),
            )
            .when_some(status, |panel, status| {
                panel.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(status),
                )
            })
            .when_some(error, |panel, error| {
                panel.child(div().w_full().text_color(theme.error).child(error))
            })
            .child(phase_body)
            .into_any_element()
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

/// 会话短标识只用于区分捕获目标，不从当前焦点或后来选中的标签推导身份。
///
/// @author kongweiguang
fn short_session_id(session_id: &kerminal_domain::identity::SessionId) -> SharedString {
    let short = session_id
        .as_str()
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    short.into()
}

/// 运行目标摘要只显示协议类型，不把地址、目录或凭据带入确认面板。
///
/// @author kongweiguang
fn run_target_label(target: SnippetRunTargetSummary) -> SharedString {
    match target {
        SnippetRunTargetSummary::Local => crate::locale::text("本地终端", "Local terminal"),
        SnippetRunTargetSummary::Ssh => crate::locale::text("SSH 终端", "SSH terminal"),
        SnippetRunTargetSummary::Container => crate::locale::text("容器终端", "Container terminal"),
        SnippetRunTargetSummary::Telnet => crate::locale::text("Telnet 终端", "Telnet terminal"),
        SnippetRunTargetSummary::Serial => crate::locale::text("串口终端", "Serial terminal"),
    }
}

/// 风险说明保留领域分类，包括显式未知，避免 renderer 将未知状态包装成安全状态。
///
/// @author kongweiguang
fn run_risk_label(risk: SnippetRisk) -> SharedString {
    match risk {
        SnippetRisk::Inspect => crate::locale::text("只读", "Inspect"),
        SnippetRisk::Change => crate::locale::text("变更", "Change"),
        SnippetRisk::Destructive => crate::locale::text("破坏性", "Destructive"),
        SnippetRisk::Unknown => crate::locale::text("未知风险", "Unknown risk"),
    }
}

/// 将目标和片段等冻结信息统一排版，避免确认内容被当前 workspace 状态替换。
///
/// @author kongweiguang
fn run_labeled_value(
    label: SharedString,
    value: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(theme.foreground)
                .child(value),
        )
}

/// 将可信评估转换为简短结论和具体约束原因，不推测当前系统或 shell 名称。
///
/// @author kongweiguang
fn applicability_copy(
    projection: &SnippetApplicabilityProjection,
    theme: ThemeTokens,
) -> (SharedString, Option<SharedString>, gpui_kit::Hsla) {
    match projection.assessment.status {
        SnippetApplicabilityStatus::Unrestricted => (
            crate::locale::text(
                "未声明平台或 shell 限制",
                "No platform or shell restriction declared",
            ),
            None,
            theme.muted,
        ),
        SnippetApplicabilityStatus::Compatible => (
            crate::locale::text(
                "当前目标符合片段声明",
                "The current target matches the declaration",
            ),
            Some(crate::locale::text(
                "可核对的适用性约束与当前已验证目标相符。",
                "The applicability constraints that can be checked match the verified target.",
            )),
            theme.success,
        ),
        SnippetApplicabilityStatus::Unknown => (
            crate::locale::text("适用性未确认", "Applicability is unconfirmed"),
            Some(applicability_reason_copy(projection)),
            theme.warning,
        ),
        SnippetApplicabilityStatus::Incompatible => (
            crate::locale::text(
                "不适用于当前已验证终端",
                "Not compatible with the verified terminal",
            ),
            Some(applicability_reason_copy(projection)),
            theme.error,
        ),
    }
}

/// 解释未确认或不兼容的具体原因，只引用目录原声明和 application 的 typed reason。
///
/// @author kongweiguang
fn applicability_reason_copy(projection: &SnippetApplicabilityProjection) -> SharedString {
    let declared_shells = projection.shells.join(", ");
    let declared_platforms = projection.platforms.join(", ");
    match projection.assessment.reason {
        Some(SnippetApplicabilityReason::TargetNotAvailable) => crate::locale::text(
            "没有可核验的当前终端 binding；无法确认该模板是否适用于目标。",
            "There is no verifiable terminal binding, so applicability cannot be confirmed.",
        ),
        Some(SnippetApplicabilityReason::PlatformNotReported) => crate::locale::text(
            format!("当前连接未报告可信操作系统；目录声明的平台为：{declared_platforms}。"),
            format!(
                "The connection did not report a trusted OS; declared platforms: {declared_platforms}."
            ),
        ),
        Some(SnippetApplicabilityReason::ExactShellNotReported) => crate::locale::text(
            format!("无法确认目录声明的具体 shell family：{declared_shells}。"),
            format!("The declared shell family could not be confirmed: {declared_shells}."),
        ),
        Some(SnippetApplicabilityReason::ShellDialectMismatch) => crate::locale::text(
            format!(
                "目录声明仅支持 {declared_shells}，与当前已验证 shell 方言不兼容；运行已禁用。"
            ),
            format!(
                "The declaration supports {declared_shells}, which conflicts with the verified shell dialect; Run is disabled."
            ),
        ),
        None => crate::locale::text(
            "应用未能证明该模板适用于当前目标。",
            "The application could not prove this snippet applies to the current target.",
        ),
    }
}

/// 适用性声明来自 catalog projection，不对目录行逐条发起会话查询。
///
/// @author kongweiguang
fn applicability_declarations(projection: &SnippetApplicabilityProjection) -> Option<SharedString> {
    let platforms = (!projection.platforms.is_empty()).then(|| {
        crate::locale::text(
            format!("平台：{}", projection.platforms.join(", ")),
            format!("Platforms: {}", projection.platforms.join(", ")),
        )
    });
    let shells = (!projection.shells.is_empty()).then(|| {
        crate::locale::text(
            format!("Shell：{}", projection.shells.join(", ")),
            format!("Shells: {}", projection.shells.join(", ")),
        )
    });
    match (platforms, shells) {
        (Some(platforms), Some(shells)) => Some(crate::locale::text(
            format!("{platforms} · {shells}"),
            format!("{platforms} · {shells}"),
        )),
        (Some(declarations), None) | (None, Some(declarations)) => Some(declarations),
        (None, None) => None,
    }
}

/// 运行回执只展示 typed 完成、usage 和 history 状态，不输出内部错误或命令内容。
///
/// @author kongweiguang
fn render_run_result(result: &SnippetRunResult, theme: ThemeTokens) -> gpui_kit::AnyElement {
    match result {
        SnippetRunResult::Finished {
            exit_code,
            usage,
            history,
            ..
        } => {
            let exit_label = crate::locale::text(
                format!("命令退出码：{exit_code}"),
                format!("Command exit code: {exit_code}"),
            );
            let exit_color = if *exit_code == 0 {
                theme.success
            } else {
                theme.warning
            };
            let usage_label = match usage {
                SnippetRunUsage::Recorded(_) => crate::locale::text("已记录", "Recorded"),
                SnippetRunUsage::NotRecorded { .. } => {
                    crate::locale::text("未记录用量", "Usage not recorded")
                }
            };
            let history_label = match history {
                SnippetRunHistory::NotRequested => {
                    crate::locale::text("未请求历史记录", "History not requested")
                }
                SnippetRunHistory::SkippedSensitive => crate::locale::text(
                    "敏感命令未写入历史",
                    "Sensitive command skipped in history",
                ),
                SnippetRunHistory::Evaluated(record) if record.recorded => {
                    crate::locale::text("已写入历史", "Added to history")
                }
                SnippetRunHistory::Evaluated(_) => {
                    crate::locale::text("历史策略未记录", "History policy skipped recording")
                }
                SnippetRunHistory::Failed { .. } => {
                    crate::locale::text("历史记录未能保存", "History could not be saved")
                }
            };

            div()
                .id("snippet-run-result")
                .w_full()
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .rounded_md()
                .bg(theme.materials.panel)
                .child(div().text_color(exit_color).child(exit_label))
                .child(run_labeled_value(
                    crate::locale::text("用量", "Usage"),
                    usage_label,
                    theme,
                ))
                .child(run_labeled_value(
                    crate::locale::text("历史", "History"),
                    history_label,
                    theme,
                ))
                .into_any_element()
        }
        SnippetRunResult::Unknown { .. } => phase_status_panel(
            "snippet-run-unknown-result",
            crate::locale::text(
                "终端回执未知。请检查原目标终端；不要直接重试。",
                "The terminal receipt is unknown. Inspect the original terminal; do not retry directly.",
            ),
            theme,
        ),
    }
}

/// 阶段提示使用独立 ID 与中性材料，避免把未知或取消伪装成命令执行成功。
///
/// @author kongweiguang
fn phase_status_panel(
    id: &'static str,
    message: SharedString,
    theme: ThemeTokens,
) -> gpui_kit::AnyElement {
    div()
        .id(id)
        .w_full()
        .p_3()
        .rounded_md()
        .bg(theme.materials.panel)
        .text_color(theme.muted)
        .child(message)
        .into_any_element()
}

/// 显式标签保持字段含义可见，输入值不会只依赖 placeholder 才能辨认。
///
/// @author kongweiguang
fn field_label(
    label: gpui_kit::SharedString,
    input: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(input)
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
