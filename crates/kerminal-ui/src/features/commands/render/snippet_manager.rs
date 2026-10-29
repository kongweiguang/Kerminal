//! 片段库管理模态只组合已有的只读校验与目录请求，不在 UI 解析或写入配置。
//!
//! @author kongweiguang

use crate::theme::ThemeTokens;
use gpui_kit::{
    AnyElement, ClickEvent, ClipboardItem, Context, IntoElement, StatefulInteractiveElement as _,
    Window,
    assets::IconName,
    component::{
        Disableable, Icon,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};

use super::super::CommandsView;

impl CommandsView {
    /// 在共享 Root modal 中复用同一 CommandsView 表单，避免复制状态或改写管理用例。
    /// 右栏弹层继续使用局部中性色阶，提升到 Root 后也不能回退成工作台的蓝灰标签。
    ///
    /// @author kongweiguang
    pub(crate) fn render_snippet_overlay_modal(
        &mut self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = if self.compact_layout {
            theme.for_tool_panel()
        } else {
            theme
        };
        if self.snippet_library_manager_open {
            self.render_snippet_library_manager(theme, cx)
                .into_any_element()
        } else if self.snippet_import_review.is_some() {
            self.render_snippet_import_review(theme, cx)
                .into_any_element()
        } else {
            self.render_snippet_editor_modal(theme, window, cx)
                .into_any_element()
        }
    }

    /// 操作和对应路径/历史说明保持同一行，避免工具区与详情区分离后让用户来回对应。
    /// 根层固定高度由 Root 管理，内容只滚动四个操作行；底栏保留 main 的完成入口。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_library_manager(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let validation_busy = self.snippet_validation_running;
        let directory_busy = self.snippet_directory_request_pending;
        let usage_clear_busy = self.snippet_usage_clear_running;
        let history_loading = self.snippet_history_loading;
        let manager_busy = self.snippet_library_manager_busy();
        let configuration_directory = self.configuration_actions.directory.clone();
        let validator_command = self.configuration_actions.validator_command.clone();
        let history_target = self.snippet_history_target.clone();
        let history_loaded = self.snippet_history_loaded;
        let history_is_empty = self.snippet_history_entries.is_empty();
        let clear_confirm = self.snippet_usage_clear_confirm;
        let manager_status = self.status.clone();
        let history_rows = self
            .snippet_history_entries
            .iter()
            .map(|entry| {
                let entry_id = entry.id.clone();
                let command = entry.command.clone();
                let target = history_target.clone();
                Button::new(format!("snippet-manager-history-{}", entry.id))
                    .compact()
                    .ghost()
                    .w_full()
                    .h(px(32.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family("monospace")
                    .tooltip(command.clone())
                    .label(command)
                    .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                        view.select_snippet_history_entry(&entry_id, target.as_ref(), window, cx);
                    }))
            })
            .collect::<Vec<_>>();
        div()
            .id("snippet-library-manager")
            .size_full()
            .flex()
            .flex_col()
            .min_h(px(0.))
            .overflow_hidden()
            .text_color(theme.foreground)
            .track_focus(&self.snippet_manager_focus)
            .child(
                div()
                    .id("snippet-library-manager-header")
                    .w_full()
                    .flex()
                    .items_start()
                    .gap_4()
                    .px_5()
                    .py_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(17.))
                            .line_height(px(24.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(crate::locale::text("片段库管理", "Snippet library")),
                    )
                    .child(
                        Button::new("snippet-manager-close")
                            .compact()
                            .ghost()
                            .w(px(36.))
                            .h(px(36.))
                            .rounded(theme.control_radius)
                            .disabled(manager_busy)
                            .icon(IconName::X)
                            .accessibility_label(crate::locale::text(
                                "关闭片段库管理",
                                "Close snippet library manager",
                            ))
                            .tooltip(crate::locale::text("关闭", "Close"))
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
                            .px_5()
                            .py_4()
                            .child(
                                snippet_manager_action_row(
                                    "snippet-manager-config-row",
                                    IconName::FolderOpen,
                                    crate::locale::text("文件化配置", "File-based configuration"),
                                    configuration_directory.clone().unwrap_or_else(|| {
                                        crate::locale::text(
                                            "配置路径暂不可用。",
                                            "Configuration path is unavailable.",
                                        )
                                        .to_string()
                                    }),
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            Button::new("snippet-manager-copy-directory")
                                                .compact()
                                                .ghost()
                                                .w(px(32.))
                                                .h(px(32.))
                                                .icon(IconName::Copy)
                                                .accessibility_label(crate::locale::text(
                                                    "复制片段配置路径",
                                                    "Copy snippet configuration path",
                                                ))
                                                .tooltip(crate::locale::text(
                                                    "复制路径",
                                                    "Copy path",
                                                ))
                                                .disabled(configuration_directory.is_none())
                                                .on_click(cx.listener(
                                                    |view, _: &ClickEvent, _, cx| {
                                                        let Some(directory) = view
                                                            .configuration_actions
                                                            .directory
                                                            .clone()
                                                        else {
                                                            return;
                                                        };
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(directory),
                                                        );
                                                        view.status = Some(
                                                            crate::locale::text(
                                                                "片段配置路径已复制。",
                                                                "Snippet configuration path copied.",
                                                            )
                                                            .to_string(),
                                                        );
                                                        cx.notify();
                                                    },
                                                )),
                                        )
                                        .child(
                                            Button::new("snippet-manager-open-directory")
                                                .compact()
                                                .secondary()
                                                .icon(IconName::FolderOpen)
                                                .label(if directory_busy {
                                                    crate::locale::text("请求中", "Requesting")
                                                } else {
                                                    crate::locale::text(
                                                        "打开目录",
                                                        "Open folder",
                                                    )
                                                })
                                                .loading(directory_busy)
                                                .disabled(
                                                    manager_busy
                                                        || configuration_directory.is_none(),
                                                )
                                                .on_click(cx.listener(
                                                    |view, _: &ClickEvent, _, cx| {
                                                        view.open_snippet_configuration_directory(cx);
                                                    },
                                                )),
                                        ),
                                    theme,
                                ),
                            )
                            .child(
                                snippet_manager_action_row(
                                    "snippet-manager-validator-row",
                                    IconName::ShieldCheck,
                                    crate::locale::text("配置校验", "Configuration validation"),
                                    validator_command.clone().unwrap_or_else(|| {
                                        crate::locale::text(
                                            "使用 Kerminal 配置加载器校验当前片段目录。",
                                            "Use the Kerminal configuration loader to validate the current snippet directory.",
                                        )
                                        .to_string()
                                    }),
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .when(validator_command.is_some(), |actions| {
                                            actions.child(
                                                Button::new("snippet-manager-copy-validator")
                                                    .compact()
                                                    .ghost()
                                                    .w(px(32.))
                                                    .h(px(32.))
                                                    .icon(IconName::Copy)
                                                    .accessibility_label(crate::locale::text(
                                                        "复制配置校验命令",
                                                        "Copy configuration validator command",
                                                    ))
                                                    .tooltip(crate::locale::text(
                                                        "复制校验命令",
                                                        "Copy validator command",
                                                    ))
                                                    .on_click(cx.listener(
                                                        |view, _: &ClickEvent, _, cx| {
                                                            let Some(command) = view
                                                                .configuration_actions
                                                                .validator_command
                                                                .clone()
                                                            else {
                                                                return;
                                                            };
                                                            cx.write_to_clipboard(
                                                                ClipboardItem::new_string(command),
                                                            );
                                                            view.status = Some(
                                                                crate::locale::text(
                                                                    "配置校验命令已复制。",
                                                                    "Configuration validator command copied.",
                                                                )
                                                                .to_string(),
                                                            );
                                                            cx.notify();
                                                        },
                                                    )),
                                            )
                                        })
                                        .child(
                                            Button::new("snippet-manager-validate")
                                                .compact()
                                                .secondary()
                                                .icon(IconName::ShieldCheck)
                                                .label(if validation_busy {
                                                    crate::locale::text("正在校验", "Validating")
                                                } else {
                                                    crate::locale::text(
                                                        "校验配置",
                                                        "Validate configuration",
                                                    )
                                                })
                                                .loading(validation_busy)
                                                .disabled(manager_busy)
                                                .on_click(cx.listener(
                                                    |view, _: &ClickEvent, _, cx| {
                                                        view.validate_snippet_configuration(cx);
                                                    },
                                                )),
                                        ),
                                    theme,
                                ),
                            )
                            .child(
                                div()
                                    .id("snippet-manager-history-picker")
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .child(snippet_manager_action_row(
                                        "snippet-manager-history-row",
                                        IconName::RotateCcwClock,
                                        crate::locale::text("命令历史", "Command history"),
                                        crate::locale::text(
                                            "从当前分屏最近命令创建可编辑副本。",
                                            "Create an editable copy from a recent command in the focused pane.",
                                        ),
                                        Button::new("snippet-manager-load-history")
                                            .compact()
                                            .secondary()
                                            .icon(IconName::ListClock)
                                            .label(if history_loading {
                                                crate::locale::text("正在读取", "Loading")
                                            } else {
                                                crate::locale::text(
                                                    "选择历史命令",
                                                    "Choose a history command",
                                                )
                                            })
                                            .loading(history_loading)
                                            .disabled(manager_busy)
                                            .on_click(cx.listener(
                                                |view, _: &ClickEvent, _, cx| {
                                                    view.load_snippet_history(cx);
                                                },
                                            )),
                                        theme,
                                    ))
                                    .when(history_loaded && history_is_empty, |section| {
                                        section.child(
                                            div()
                                                .id("snippet-manager-history-empty")
                                                .w_full()
                                                .py_2()
                                                .text_size(theme.text_xs)
                                                .text_color(theme.muted)
                                                .child(crate::locale::text(
                                                    "没有可保存的命令。",
                                                    "There are no commands to save.",
                                                )),
                                        )
                                    })
                                    .children(history_rows),
                            )
                            .child(
                                div()
                                    .id("snippet-manager-usage-clear")
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .child(snippet_manager_action_row(
                                        "snippet-manager-usage-row",
                                        IconName::ListRestart,
                                        crate::locale::text("最近使用", "Recent usage"),
                                        crate::locale::text(
                                            "收藏项不会被移除。",
                                            "Favorites will be kept.",
                                        ),
                                        if clear_confirm {
                                            div()
                                                .id("snippet-manager-usage-clear-confirm")
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    Button::new("snippet-manager-usage-clear-cancel")
                                                        .compact()
                                                        .ghost()
                                                        .disabled(usage_clear_busy)
                                                        .label(crate::locale::text("取消", "Cancel"))
                                                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                                            view.cancel_clear_snippet_usage(cx);
                                                        })),
                                                )
                                                .child(
                                                    Button::new("snippet-manager-usage-clear-confirm")
                                                        .compact()
                                                        .danger()
                                                        .loading(usage_clear_busy)
                                                        .disabled(usage_clear_busy)
                                                        .label(if usage_clear_busy {
                                                            crate::locale::text("正在清除", "Clearing")
                                                        } else {
                                                            crate::locale::text("确认清除", "Clear")
                                                        })
                                                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                                            view.clear_snippet_usage(cx);
                                                        })),
                                                )
                                                .into_any_element()
                                        } else {
                                            Button::new("snippet-manager-usage-clear-request")
                                                .compact()
                                                .ghost()
                                                .icon(IconName::ListRestart)
                                                .disabled(manager_busy)
                                                .label(crate::locale::text(
                                                    "清除最近",
                                                    "Clear recent",
                                                ))
                                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                                    view.request_clear_snippet_usage_confirmation(cx);
                                                }))
                                                .into_any_element()
                                        },
                                        theme,
                                    )),
                            )
                            .when_some(manager_status, |content, status| {
                                content.child(
                                    div()
                                        .id("snippet-manager-status")
                                        .w_full()
                                        .py_2()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.foreground)
                                        .child(status),
                                )
                            })
                            ,
                    ),
            )
            .child(
                div()
                    .id("snippet-library-manager-footer")
                    .w_full()
                    .flex_shrink_0()
                    .px_5()
                    .py_4()
                    .flex()
                    .items_center()
                    .justify_end()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.materials.chrome)
                    .rounded_b(theme.radius_dialog)
                    .child(
                        Button::new("snippet-manager-done")
                            .primary()
                            .label(crate::locale::text("完成", "Done"))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                if view.snippet_validation_running
                                    || view.snippet_directory_request_pending
                                {
                                    view.status = None;
                                }
                                view.finish_snippet_library_manager(cx);
                            })),
                    ),
            )
    }
}

/// 让动作持续贴近其对象说明，并为路径等长内容提供可收缩的单行文本区。
/// 管理器中的加载和失败状态仍归 CommandsView 持有，行组件只负责一致的几何层级。
///
/// @author kongweiguang
fn snippet_manager_action_row(
    id: &'static str,
    icon: IconName,
    title: impl IntoElement,
    detail: impl IntoElement,
    action: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_3()
        .py_3()
        .border_b_1()
        .border_color(theme.border)
        .child(Icon::new(icon).size(px(16.)).text_color(theme.muted))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(title),
                )
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(11.))
                        .line_height(px(16.))
                        .text_color(theme.muted)
                        .child(detail),
                ),
        )
        .child(div().flex_shrink_0().child(action))
}
