//! 片段补充元数据与上下文绑定编辑器。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::Input,
        menu::{DropdownMenu as _, PopupMenuItem},
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::commands::{
    SnippetContextBindingKind, SnippetDefaultAction, SnippetRisk, SnippetScope,
};

use crate::theme::ThemeTokens;

use super::super::CommandsView;

impl CommandsView {
    /// 将低频元数据收在可展开区域，并返回拥有化元素，避免与主表单后续输入字段争用视图借用。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_advanced(
        &mut self,
        theme: ThemeTokens,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let open = self.snippet_advanced_open;
        let saving = self.snippet_saving;
        let body = open.then(|| {
            div()
                .id("snippet-advanced-fields")
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .pt_3()
                .child(field_label(
                    crate::locale::text("说明", "Description"),
                    Input::new(&self.snippet_description)
                        .disabled(!writable || saving)
                        .w_full(),
                    theme,
                ))
                .child(field_label(
                    crate::locale::text("标签（逗号分隔）", "Tags (comma separated)"),
                    Input::new(&self.snippet_tags)
                        .disabled(!writable || saving)
                        .w_full(),
                    theme,
                ))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_3()
                        .child(field_label(
                            crate::locale::text("分类", "Category"),
                            Input::new(&self.snippet_category)
                                .disabled(!writable || saving)
                                .w_full(),
                            theme,
                        ))
                        .child(field_label(
                            crate::locale::text("排序值", "Sort order"),
                            Input::new(&self.snippet_sort_order)
                                .disabled(!writable || saving)
                                .w_full(),
                            theme,
                        )),
                )
                .child(self.render_snippet_policy_controls(theme, writable, cx))
                .child(self.render_snippet_context_bindings(theme, writable, cx))
                .when(!writable && !self.snippet_variables.is_empty(), |body| {
                    body.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(section_title(
                                crate::locale::text("变量定义", "Variable definitions"),
                                theme,
                            ))
                            .children(
                                self.render_snippet_variable_definitions(theme, writable, cx),
                            ),
                    )
                })
        });

        div()
            .id("snippet-advanced-editor")
            .w_full()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Button::new("snippet-advanced-toggle")
                    .compact()
                    .ghost()
                    .label(crate::locale::text(
                        if open {
                            "收起高级设置"
                        } else {
                            "更多设置"
                        },
                        if open {
                            "Hide advanced settings"
                        } else {
                            "More settings"
                        },
                    ))
                    .disabled(saving)
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.toggle_snippet_advanced(cx);
                    })),
            )
            .when_some(body, |section, body| section.child(body))
            .into_any_element()
    }

    /// 低频元数据区集中展示策略，具体候选由菜单明确列出，避免用户靠连续循环猜选项。
    ///
    /// @author kongweiguang
    fn render_snippet_policy_controls(
        &mut self,
        theme: ThemeTokens,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let scope = self.snippet_scope;
        let risk = self.snippet_risk;
        let action = self.snippet_default_action;
        let scope_label = scope_label(scope);
        let risk_label = risk_label(risk);
        let action_label = default_action_label(action);
        let disabled = !writable || self.snippet_saving;
        let scope_view = cx.entity().clone();
        let risk_view = cx.entity().clone();
        let action_view = cx.entity().clone();
        div()
            .id("snippet-policy-controls")
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .text_color(theme.foreground)
            .child(
                Button::new("snippet-scope-cycle")
                    .compact()
                    .ghost()
                    .dropdown_caret(true)
                    .label(crate::locale::text(
                        format!("范围 · {scope_label}"),
                        format!("Scope · {scope_label}"),
                    ))
                    .disabled(disabled)
                    .dropdown_menu(move |mut menu, _, _| {
                        for (candidate, label) in [
                            (SnippetScope::Any, crate::locale::text("通用", "Any")),
                            (SnippetScope::Local, crate::locale::text("本地", "Local")),
                            (SnippetScope::Ssh, crate::locale::text("SSH", "SSH")),
                        ] {
                            let target = scope_view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(label)
                                    .checked(candidate == scope)
                                    .on_click(move |_, _, app| {
                                        target.update(app, |view, cx| {
                                            view.set_snippet_scope(candidate, cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .child(
                Button::new("snippet-risk-cycle")
                    .compact()
                    .ghost()
                    .dropdown_caret(true)
                    .label(crate::locale::text(
                        format!("风险 · {risk_label}"),
                        format!("Risk · {risk_label}"),
                    ))
                    .disabled(disabled)
                    .dropdown_menu(move |mut menu, _, _| {
                        for (candidate, label) in [
                            (SnippetRisk::Inspect, crate::locale::text("只读", "Inspect")),
                            (SnippetRisk::Change, crate::locale::text("变更", "Change")),
                            (
                                SnippetRisk::Destructive,
                                crate::locale::text("破坏性", "Destructive"),
                            ),
                            (SnippetRisk::Unknown, crate::locale::text("未知", "Unknown")),
                        ] {
                            let target = risk_view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(label)
                                    .checked(candidate == risk)
                                    .on_click(move |_, _, app| {
                                        target.update(app, |view, cx| {
                                            view.set_snippet_risk(candidate, cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .child(
                Button::new("snippet-default-action-cycle")
                    .compact()
                    .ghost()
                    .dropdown_caret(true)
                    .label(crate::locale::text(
                        format!("默认动作 · {action_label}"),
                        format!("Default action · {action_label}"),
                    ))
                    .disabled(disabled)
                    .dropdown_menu(move |mut menu, _, _| {
                        for (candidate, label) in [
                            (
                                SnippetDefaultAction::Insert,
                                crate::locale::text("填入草稿", "Insert into draft"),
                            ),
                            (
                                SnippetDefaultAction::Run,
                                crate::locale::text("运行", "Run"),
                            ),
                        ] {
                            let target = action_view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(label)
                                    .checked(candidate == action)
                                    .on_click(move |_, _, app| {
                                        target.update(app, |view, cx| {
                                            view.set_snippet_default_action(candidate, cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
    }

    /// 绑定目标和类型保留为独立输入，Global 不读取或猜测当前工作区目标。
    ///
    /// @author kongweiguang
    fn render_snippet_context_bindings(
        &mut self,
        theme: ThemeTokens,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let bindings = self
            .snippet_context_bindings
            .iter()
            .enumerate()
            .map(|(index, binding)| {
                let kind = binding.binding.kind;
                let target_required = kind != SnippetContextBindingKind::Global;
                div()
                    .id(format!("snippet-context-binding-{index}"))
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(format!("snippet-context-binding-kind-{index}"))
                            .compact()
                            .label(binding_kind_label(kind))
                            .disabled(!writable || self.snippet_saving)
                            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                view.cycle_snippet_context_binding(index, cx);
                            })),
                    )
                    .when(target_required, |row| {
                        row.child(
                            Input::new(&binding.target_id)
                                .disabled(!writable || self.snippet_saving)
                                .w_full(),
                        )
                    })
                    .child(
                        Button::new(format!("snippet-context-binding-remove-{index}"))
                            .compact()
                            .ghost()
                            .label(crate::locale::text("移除", "Remove"))
                            .disabled(!writable || self.snippet_saving)
                            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                view.remove_snippet_context_binding(index, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();
        let can_add = writable && !self.snippet_saving;

        div()
            .id("snippet-context-bindings")
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme.text_sm)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(crate::locale::text("上下文绑定", "Context bindings")),
                    )
                    .child(
                        Button::new("snippet-context-binding-add")
                            .compact()
                            .label(crate::locale::text("添加绑定", "Add binding"))
                            .disabled(!can_add)
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                view.add_snippet_context_binding(window, cx);
                            })),
                    ),
            )
            .children(bindings)
            .when(
                bindings_is_empty(&self.snippet_context_bindings),
                |section| {
                    section.child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "未添加绑定时沿用目录默认范围。",
                                "Without bindings, the catalog default scope applies.",
                            )),
                    )
                },
            )
    }
}

/// 字段标签为低频选项提供常驻名称，避免用户必须猜测 placeholder。
///
/// @author kongweiguang
fn field_label(
    label: gpui_kit::SharedString,
    input: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(180.))
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

/// 变量定义标题局部绘制，避免依赖片段弹层的私有 renderer helper。
///
/// @author kongweiguang
fn section_title(label: gpui_kit::SharedString, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_sm)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(label)
}

/// 来源标签仅映射领域枚举，未知值不会被伪装成通用范围。
///
/// @author kongweiguang
fn binding_kind_label(kind: SnippetContextBindingKind) -> gpui_kit::SharedString {
    match kind {
        SnippetContextBindingKind::Global => crate::locale::text("全局", "Global"),
        SnippetContextBindingKind::Workspace => crate::locale::text("工作区", "Workspace"),
        SnippetContextBindingKind::Host => crate::locale::text("主机", "Host"),
        SnippetContextBindingKind::HostGroup => crate::locale::text("主机组", "Host group"),
    }
}

/// 范围文字直接对应允许的目标族，不改变底层约束。
///
/// @author kongweiguang
fn scope_label(scope: SnippetScope) -> gpui_kit::SharedString {
    match scope {
        SnippetScope::Any => crate::locale::text("通用", "Any"),
        SnippetScope::Local => crate::locale::text("本地", "Local"),
        SnippetScope::Ssh => crate::locale::text("SSH", "SSH"),
    }
}

/// 风险级别保持与领域值一一对应，包括显式未知状态。
///
/// @author kongweiguang
fn risk_label(risk: SnippetRisk) -> gpui_kit::SharedString {
    match risk {
        SnippetRisk::Inspect => crate::locale::text("只读", "Inspect"),
        SnippetRisk::Change => crate::locale::text("变更", "Change"),
        SnippetRisk::Destructive => crate::locale::text("破坏性", "Destructive"),
        SnippetRisk::Unknown => crate::locale::text("未知", "Unknown"),
    }
}

/// 默认动作只显示声明值，不触发插入或执行。
///
/// @author kongweiguang
fn default_action_label(action: SnippetDefaultAction) -> gpui_kit::SharedString {
    match action {
        SnippetDefaultAction::Insert => crate::locale::text("填入草稿", "Insert"),
        SnippetDefaultAction::Run => crate::locale::text("运行", "Run"),
    }
}

/// 空绑定说明只在确实无项目时出现，避免遗漏已存在的目标绑定。
///
/// @author kongweiguang
fn bindings_is_empty(bindings: &[super::super::SnippetContextBindingInput]) -> bool {
    bindings.is_empty()
}
