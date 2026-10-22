//! POSIX mode 编辑行使用主题 token 和 Kit 输入/按钮控件。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement, StatefulInteractiveElement as _,
    component::{
        Disableable as _,
        button::{Button, ButtonVariants as _},
        input::Input,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};

use crate::{locale, theme::ThemeTokens};

use super::super::FilesPane;

/// 展示权限输入、当前真实 mode 和明确的不可用原因，不为缺失 metadata 猜默认值；紧凑提示用 Note 节点保留完整辅助说明。
///
/// @author kongweiguang
pub(super) fn render_permissions(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let selected = pane.panels[pane.active_side].selected.as_ref();
    let Some(selected) = selected else {
        return div().h(px(0.)).into_any_element();
    };
    let enabled = pane.permissions.can_edit(Some(selected));
    let show_input = pane.permissions.can_show_editor(Some(selected));
    let explanation = pane.permissions.explanation(Some(selected));
    let compact_acl_notice = super::super::permissions::is_windows_local_acl_limitation(selected);
    let visible_explanation = if compact_acl_notice {
        locale::text(
            "Windows ACL 权限不在此编辑",
            "Windows ACL permissions are managed outside this view",
        )
        .to_string()
    } else {
        explanation.clone()
    };
    let tooltip_explanation = explanation.clone();
    let mut row = div()
        .w(px(292.))
        .min_w(px(220.))
        .flex_shrink_0()
        .px_2()
        .py_1()
        .flex()
        .when(compact_acl_notice, |row| row.h(px(30.)).items_center())
        .when(!compact_acl_notice, |row| {
            row.h(px(52.)).flex_col().justify_center().gap_1()
        })
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .text_size(theme.text_xs);
    if show_input && let Some(input) = pane.permissions.input.as_ref() {
        row = row.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(62.))
                        .text_color(theme.muted)
                        .child(locale::text("POSIX 权限", "POSIX mode")),
                )
                .child(Input::new(input).w(px(96.)))
                .child(
                    Button::new("files-apply-permissions")
                        .compact()
                        .label(locale::text("应用", "Apply"))
                        .tooltip(locale::text("应用八进制权限", "Apply octal permissions"))
                        .disabled(!enabled)
                        .when(enabled, Button::primary)
                        .on_click(
                            cx.listener(|pane, _: &ClickEvent, _, cx| pane.apply_permissions(cx)),
                        ),
                ),
        );
    }
    row.child(
        div()
            .id("files-permissions-explanation")
            .min_w(px(0.))
            .flex_1()
            .overflow_hidden()
            .text_size(theme.text_xs)
            .text_color(theme.subtle)
            .text_ellipsis()
            .role(gpui_kit::Role::Note)
            .aria_label(visible_explanation.clone())
            .aria_description(explanation)
            .tooltip(move |window, cx| Tooltip::new(tooltip_explanation.clone()).build(window, cx))
            .child(visible_explanation),
    )
    .into_any_element()
}
