//! 片段库管理页只组合已有的只读校验与目录请求，不在 UI 解析或写入配置。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, StatefulInteractiveElement as _,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::ConfigurationValidationSeverity;

use crate::theme::ThemeTokens;

use super::super::CommandsView;

impl CommandsView {
    /// 把管理 flag 接到独立右栏页面；异步副作用和报告数据仍归现有 transfer 用例所有。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_library_manager(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let validation_busy = self.snippet_validation_running;
        let directory_busy = self.snippet_directory_request_pending;
        let manager_busy = validation_busy || directory_busy;
        let has_report = self.snippet_validation_report.is_some();
        let report = self.snippet_validation_report.clone().map(|report| {
            let outcome = if report.valid {
                if report.warning_count == 0 {
                    crate::locale::text("未发现问题", "No issues found")
                } else {
                    crate::locale::text("未发现错误", "No errors found")
                }
            } else {
                crate::locale::text("发现配置错误", "Configuration errors found")
            };
            let outcome_color = if report.valid {
                theme.success
            } else {
                theme.error
            };
            let summary = crate::locale::text(
                format!(
                    "检查 {} 项 · {} 个错误 · {} 个警告",
                    report.checked.len(),
                    report.error_count,
                    report.warning_count
                ),
                format!(
                    "{} checked · {} errors · {} warnings",
                    report.checked.len(),
                    report.error_count,
                    report.warning_count
                ),
            );
            let diagnostics_empty = report.diagnostics.is_empty();
            let diagnostic_rows = report
                .diagnostics
                .into_iter()
                .enumerate()
                .map(|(index, diagnostic)| {
                    let (severity, severity_color) = match diagnostic.severity {
                        ConfigurationValidationSeverity::Error => {
                            (crate::locale::text("错误", "Error"), theme.error)
                        }
                        ConfigurationValidationSeverity::Warning => {
                            (crate::locale::text("警告", "Warning"), theme.warning)
                        }
                    };
                    let location = match (diagnostic.line, diagnostic.column) {
                        (Some(line), Some(column)) => {
                            format!("{}:{line}:{column}", diagnostic.path)
                        }
                        (Some(line), None) => format!("{}:{line}", diagnostic.path),
                        _ => diagnostic.path,
                    };
                    let message = diagnostic.message;
                    let recovery = diagnostic.recovery;
                    div()
                        .id(format!("snippet-manager-diagnostic-{index}"))
                        .w_full()
                        .flex()
                        .items_start()
                        .gap_2()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(theme.text_xs)
                                .text_color(severity_color)
                                .child(severity),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_wrap()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(location),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .text_size(theme.text_body)
                                        .text_color(theme.foreground)
                                        .child(message),
                                )
                                .when_some(recovery, |row, recovery| {
                                    row.child(
                                        div()
                                            .w_full()
                                            .text_size(theme.text_xs)
                                            .text_color(theme.muted)
                                            .child(recovery),
                                    )
                                }),
                        )
                })
                .collect::<Vec<_>>();
            let has_diagnostics = !diagnostic_rows.is_empty();
            let checked_rows = report
                .checked
                .into_iter()
                .enumerate()
                .map(|(index, check)| {
                    div()
                        .id(format!("snippet-manager-checked-{index}"))
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .w_full()
                                .text_size(theme.text_body)
                                .text_color(theme.foreground)
                                .child(check.path),
                        )
                        .child(
                            div()
                                .w_full()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(check.message),
                        )
                })
                .collect::<Vec<_>>();
            let has_checked = !checked_rows.is_empty();

            div()
                .id("snippet-manager-report")
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .id("snippet-manager-report-summary")
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .pb_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .text_size(theme.text_heading)
                                .child(crate::locale::text(
                                    "最近一份校验报告",
                                    "Latest validation report",
                                ))
                                .child(div().text_color(outcome_color).child(outcome)),
                        )
                        .child(
                            div()
                                .w_full()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(summary),
                        ),
                )
                .when(diagnostics_empty, |view| {
                    view.child(
                        div()
                            .id("snippet-manager-no-diagnostics")
                            .w_full()
                            .text_size(theme.text_body)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "没有诊断项。",
                                "There are no diagnostics.",
                            )),
                    )
                })
                .when(has_diagnostics, |view| {
                    view.child(
                        div()
                            .id("snippet-manager-diagnostics")
                            .w_full()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .w_full()
                                    .py_1()
                                    .text_size(theme.text_sm)
                                    .text_color(theme.foreground)
                                    .child(crate::locale::text("诊断", "Diagnostics")),
                            )
                            .children(diagnostic_rows),
                    )
                })
                .when(has_checked, |view| {
                    view.child(
                        div()
                            .id("snippet-manager-checked-items")
                            .w_full()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .w_full()
                                    .py_1()
                                    .text_size(theme.text_sm)
                                    .text_color(theme.foreground)
                                    .child(crate::locale::text("已检查", "Checked")),
                            )
                            .children(checked_rows),
                    )
                })
        });

        div()
            .id("snippet-library-manager")
            .size_full()
            .flex()
            .flex_col()
            .min_h(px(0.))
            .overflow_hidden()
            .bg(theme.materials.shell)
            .text_color(theme.foreground)
            .child(
                div()
                    .id("snippet-library-manager-header")
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(theme.text_heading)
                                    .child(crate::locale::text("片段库管理", "Snippet library")),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(crate::locale::text(
                                        "校验本机片段配置，或打开配置目录。",
                                        "Validate local snippet configuration or open its folder.",
                                    )),
                            ),
                    )
                    .child(
                        Button::new("snippet-manager-back")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("返回片段", "Back to snippets"))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                if view.snippet_validation_running
                                    || view.snippet_directory_request_pending
                                {
                                    view.status = None;
                                }
                                view.close_snippet_library_manager(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("snippet-library-manager-actions")
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("snippet-manager-validate")
                            .compact()
                            .secondary()
                            .label(if validation_busy {
                                crate::locale::text("正在校验", "Validating")
                            } else {
                                crate::locale::text("校验配置", "Validate configuration")
                            })
                            .loading(validation_busy)
                            .disabled(manager_busy)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.validate_snippet_configuration(cx);
                            })),
                    )
                    .child(
                        Button::new("snippet-manager-open-directory")
                            .compact()
                            .ghost()
                            .label(if directory_busy {
                                crate::locale::text("请求中", "Requesting")
                            } else {
                                crate::locale::text("打开配置目录", "Open configuration folder")
                            })
                            .loading(directory_busy)
                            .disabled(manager_busy)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.open_snippet_configuration_directory(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("snippet-library-manager-content")
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .px_3()
                            .py_3()
                            .when_some(report, |view, report| view.child(report))
                            .when(!has_report, |view| {
                                view.child(
                                    div()
                                        .id("snippet-manager-no-report")
                                        .w_full()
                                        .py_2()
                                        .text_size(theme.text_body)
                                        .text_color(theme.muted)
                                        .child(crate::locale::text(
                                            "尚未运行配置校验。",
                                            "Configuration has not been validated yet.",
                                        )),
                                )
                            }),
                    ),
            )
    }
}
