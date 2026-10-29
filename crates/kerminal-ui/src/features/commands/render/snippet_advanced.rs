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
        select::Select,
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
use super::snippets::compact_snippet_field;

impl CommandsView {
    /// 范围与标题同属编辑器首行；擦除按钮的 opaque 类型，避免 Rust 2024 隐式捕获 self/cx 借用。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_scope_control(
        &mut self,
        theme: ThemeTokens,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let scope = self.snippet_scope;
        let scope_label = scope_label(scope);
        let disabled = !writable || self.snippet_saving;
        if self.compact_layout {
            return compact_snippet_field(
                Select::new(&self.snippet_scope_select)
                    .id("snippet-scope-select")
                    .w_full()
                    .placeholder(crate::locale::text("选择作用域", "Choose scope"))
                    .accessibility_label(crate::locale::text("作用域", "Scope"))
                    .disabled(disabled),
                theme,
            )
            .into_any_element();
        }
        let scope_view = cx.entity().clone();
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
                            .on_click(move |_, window, app| {
                                target.update(app, |view, cx| {
                                    view.set_snippet_scope(candidate, cx);
                                    view.sync_snippet_editor_selects(window, cx);
                                });
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    /// 把片段元数据放在命令模板前常驻展示；紧凑上下文组沿用 main 的独立 16px 表单组间距。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_editor_metadata(
        &mut self,
        theme: ThemeTokens,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let saving = self.snippet_saving;
        if self.compact_layout {
            let disabled = !writable || saving;
            let context_bindings = self.render_snippet_context_bindings(theme, writable, true, cx);
            div()
                .id("snippet-editor-metadata")
                .w_full()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_3()
                        .child(compact_field_label(
                            crate::locale::text("分类", "Category"),
                            compact_snippet_field(
                                Input::new(&self.snippet_category)
                                    .disabled(disabled)
                                    .w_full(),
                                theme,
                            ),
                            theme,
                        ))
                        .child(compact_field_label(
                            crate::locale::text("风险", "Risk"),
                            compact_snippet_field(
                                Select::new(&self.snippet_risk_select)
                                    .id("snippet-risk-select")
                                    .w_full()
                                    .placeholder(crate::locale::text("选择风险", "Choose risk"))
                                    .accessibility_label(crate::locale::text("风险", "Risk"))
                                    .disabled(disabled),
                                theme,
                            ),
                            theme,
                        ))
                        .child(compact_field_label(
                            crate::locale::text("默认动作", "Default action"),
                            compact_snippet_field(
                                Select::new(&self.snippet_default_action_select)
                                    .id("snippet-default-action-select")
                                    .w_full()
                                    .placeholder(crate::locale::text(
                                        "选择默认动作",
                                        "Choose default action",
                                    ))
                                    .accessibility_label(crate::locale::text(
                                        "默认动作",
                                        "Default action",
                                    ))
                                    .disabled(disabled),
                                theme,
                            ),
                            theme,
                        ))
                        .child(compact_field_label(
                            crate::locale::text("排序", "Sort order"),
                            compact_snippet_field(
                                Input::new(&self.snippet_sort_order)
                                    .disabled(disabled)
                                    .w_full(),
                                theme,
                            ),
                            theme,
                        )),
                )
                .child(context_bindings)
                .into_any_element()
        } else {
            div()
                .id("snippet-editor-metadata")
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
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
                            false,
                        ))
                        .child(self.render_snippet_policy_controls(theme, writable, cx))
                        .child(field_label(
                            crate::locale::text("排序值", "Sort order"),
                            Input::new(&self.snippet_sort_order)
                                .disabled(!writable || saving)
                                .w_full(),
                            theme,
                            false,
                        )),
                )
                .child(self.render_snippet_context_bindings(theme, writable, false, cx))
                .into_any_element()
        }
    }

    /// 说明和标签留在编辑表单末尾，贴合主分支的字段次序并保持完整可见。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_editor_text_details(
        &self,
        theme: ThemeTokens,
        writable: bool,
    ) -> gpui_kit::AnyElement {
        let saving = self.snippet_saving;
        let compact_layout = self.compact_layout;
        let tags_label = if self.compact_layout {
            crate::locale::text("标签", "Tags")
        } else {
            crate::locale::text("标签（逗号分隔）", "Tags (comma separated)")
        };
        div()
            .id("snippet-editor-text-details")
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(field_label(
                crate::locale::text("说明", "Description"),
                Input::new(&self.snippet_description)
                    .disabled(!writable || saving)
                    .w_full()
                    .when(compact_layout, |input| compact_snippet_field(input, theme)),
                theme,
                compact_layout,
            ))
            .child(field_label(
                tags_label,
                Input::new(&self.snippet_tags)
                    .disabled(!writable || saving)
                    .w_full()
                    .when(compact_layout, |input| compact_snippet_field(input, theme)),
                theme,
                compact_layout,
            ))
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
        let risk = self.snippet_risk;
        let action = self.snippet_default_action;
        let risk_label = risk_label(risk);
        let action_label = default_action_label(action);
        let disabled = !writable || self.snippet_saving;
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
                                    .on_click(move |_, window, app| {
                                        target.update(app, |view, cx| {
                                            view.set_snippet_risk(candidate, cx);
                                            view.sync_snippet_editor_selects(window, cx);
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
                                    .on_click(move |_, window, app| {
                                        target.update(app, |view, cx| {
                                            view.set_snippet_default_action(candidate, cx);
                                            view.sync_snippet_editor_selects(window, cx);
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
        compact_layout: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        if compact_layout {
            let kind = self
                .snippet_context_bindings
                .first()
                .map_or(SnippetContextBindingKind::Global, |binding| {
                    binding.binding.kind
                });
            let target = self
                .snippet_context_bindings
                .first()
                .filter(|_| kind != SnippetContextBindingKind::Global)
                .map(|binding| {
                    compact_field_label(
                        crate::locale::text("绑定目标 ID", "Binding target ID"),
                        compact_snippet_field(
                            Input::new(&binding.target_id)
                                .disabled(!writable || self.snippet_saving)
                                .w_full(),
                            theme,
                        ),
                        theme,
                    )
                    .into_any_element()
                });
            return div()
                .id("snippet-context-bindings")
                .w_full()
                .flex()
                .flex_wrap()
                .gap_3()
                .child(compact_field_label(
                    crate::locale::text("上下文绑定", "Context binding"),
                    compact_snippet_field(
                        Select::new(&self.snippet_context_select)
                            .id("snippet-context-binding-kind")
                            .w_full()
                            .placeholder(binding_kind_label(kind))
                            .accessibility_label(crate::locale::text(
                                "上下文绑定",
                                "Context binding",
                            ))
                            .disabled(!writable || self.snippet_saving),
                        theme,
                    ),
                    theme,
                ))
                .when_some(target, |row, target| row.child(target))
                .into_any_element();
        }

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
                            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                view.cycle_snippet_context_binding(index, window, cx);
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
                            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                view.remove_snippet_context_binding(index, window, cx);
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
            .into_any_element()
    }
}

/// 字段标签为低频选项提供常驻名称，避免用户必须猜测 placeholder。
/// 字段名称使用独立语义色，右栏根弹层与内嵌表单保持相同对比度。
///
/// @author kongweiguang
fn field_label(
    label: gpui_kit::SharedString,
    input: impl IntoElement,
    theme: ThemeTokens,
    compact_layout: bool,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(180.))
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

/// Compact 元数据固定两列，最窄字段宽度保护标签，最大宽度也让单独的 Global 控件保持半行。
/// 标签保留 main 的 24px 父行盒和实际常规中文字形，避免每组高度误差向下累积。
///
/// @author kongweiguang
fn compact_field_label(
    label: gpui_kit::SharedString,
    input: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(250.))
        .max_w(px(310.))
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(theme.text_xs)
                .line_height(px(24.))
                .font_weight(gpui_kit::FontWeight::NORMAL)
                .text_color(theme.field_label)
                .child(label),
        )
        .child(input)
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
